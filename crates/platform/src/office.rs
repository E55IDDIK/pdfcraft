//! Optional Office-to-PDF conversion through a locally installed LibreOffice process.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_INPUT: usize = 512 * 1024 * 1024;
const MAX_OUTPUT: u64 = 512 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(90);
const OFFICE_EXTENSIONS: &[&str] = &["doc", "docx", "ppt", "pptx"];

/// Whether `name` is a Word or PowerPoint file supported by the sidecar converter.
pub fn is_office_document(name: &str) -> bool {
    Path::new(name).extension().and_then(|ext| ext.to_str()).is_some_and(|ext| OFFICE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Convert an Office document using an installed LibreOffice executable.
///
/// Input and output live in a fresh temporary directory, macros are disabled for the isolated
/// LibreOffice profile, and both file sizes and process time are bounded. The caller's file is
/// never modified. This is native-only by design; browser builds do not launch sidecar processes.
pub fn office_to_pdf(name: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
    if !is_office_document(name) {
        return Err(format!("{name}: expected a Word, PowerPoint or OpenDocument file"));
    }
    if bytes.is_empty() {
        return Err(format!("{name}: the Office document is empty"));
    }
    if bytes.len() > MAX_INPUT {
        return Err(format!("{name}: Office documents larger than 512 MiB cannot be converted"));
    }

    let workspace = TempWorkspace::create()?;
    let extension = Path::new(name).extension().and_then(|ext| ext.to_str()).ok_or_else(|| format!("{name}: file extension is not valid Unicode"))?;
    let source = workspace.path.join(format!("source.{}", extension.to_ascii_lowercase()));
    let target = workspace.path.join("source.pdf");
    #[cfg(not(target_os = "windows"))]
    let profile = {
        let profile = workspace.path.join("profile");
        std::fs::create_dir(&profile).map_err(|e| format!("couldn't prepare a private LibreOffice profile: {e}"))?;
        profile
    };
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&source).map_err(|e| format!("couldn't stage {name}: {e}"))?;
    file.write_all(bytes).map_err(|e| format!("couldn't stage {name}: {e}"))?;
    file.sync_all().map_err(|e| format!("couldn't stage {name}: {e}"))?;
    drop(file);

    #[cfg(target_os = "windows")]
    run_office_helper(&source, &target).map_err(|error| format!("couldn't convert {name}: {error}"))?;
    #[cfg(not(target_os = "windows"))]
    run_libreoffice(&source, &target, &profile, name).map_err(|error| format!("couldn't convert {name}: {error}"))?;

    let mut output = Vec::new();
    std::fs::File::open(&target)
        .map_err(|e| format!("the Office suite did not produce a PDF for {name}: {e}"))?
        .take(MAX_OUTPUT + 1)
        .read_to_end(&mut output)
        .map_err(|e| format!("couldn't read the converted PDF for {name}: {e}"))?;
    if output.len() as u64 > MAX_OUTPUT {
        return Err(format!("the PDF output for {name} exceeds 512 MiB"));
    }
    if !output.starts_with(b"%PDF-") {
        return Err(format!("the Office suite produced an invalid PDF for {name}"));
    }
    Ok(output)
}

/// The helper mode runs in a child process so an Office modal dialog or hung COM server can be
/// terminated without hanging the desktop app or an automation client.
#[cfg(target_os = "windows")]
fn run_office_helper(source: &Path, target: &Path) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|e| format!("couldn't locate the PdfCraft process: {e}"))?;
    let executable_name = executable.file_stem().and_then(|name| name.to_str()).unwrap_or_default();
    if !executable_name.eq_ignore_ascii_case("pdfcraft") && !executable_name.eq_ignore_ascii_case("pdfcraft-cli") {
        return Err("Office conversion must be started by the PdfCraft desktop app or CLI".into());
    }
    let mut child = Command::new(executable)
        .arg("--pdfcraft-office-convert")
        .arg(source)
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("couldn't start the Office conversion helper: {e}"))?;
    let status = wait_bounded(&mut child)?;
    if status.success() { Ok(()) } else { Err(format!("Microsoft Office could not convert the document (process exited with {status})")) }
}

#[cfg(not(target_os = "windows"))]
fn run_libreoffice(source: &Path, target: &Path, profile: &Path, name: &str) -> Result<(), String> {
    let executable = libreoffice_executable()
        .ok_or_else(|| "Office conversion needs LibreOffice installed (the `soffice` executable was not found).".to_string())?;
    let profile_url = file_url(profile);
    let mut child = Command::new(executable)
        .arg("--headless")
        .arg("--nologo")
        .arg("--nodefault")
        .arg("--nolockcheck")
        .arg("--norestore")
        .arg("--convert-to")
        .arg("pdf")
        .arg("--outdir")
        .arg(target.parent().ok_or_else(|| "invalid temporary output path".to_string())?)
        .arg(format!("-env:UserInstallation={profile_url}"))
        .arg(source)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("couldn't start LibreOffice: {e}"))?;
    let status = wait_bounded(&mut child)?;
    if status.success() { Ok(()) } else { Err(format!("LibreOffice could not convert {name} (process exited with {status})")) }
}

/// Entry point for `--pdfcraft-office-convert`, called only in the isolated helper process.
#[cfg(target_os = "windows")]
pub fn office_to_pdf_internal(source: &Path, target: &Path) -> Result<(), String> {
    windows_office::convert(source, target)
}

#[cfg(not(target_os = "windows"))]
pub fn office_to_pdf_internal(_: &Path, _: &Path) -> Result<(), String> {
    Err("the Microsoft Office conversion helper is available only on Windows".into())
}

#[cfg(target_os = "windows")]
mod windows_office {
    use std::path::Path;
    use winsafe::{self as w, co, prelude::*};

    pub(super) fn convert(source: &Path, target: &Path) -> Result<(), String> {
        let source = source.to_str().ok_or_else(|| "the staged Office path is not Unicode".to_string())?;
        let target = target.to_str().ok_or_else(|| "the staged PDF path is not Unicode".to_string())?;
        let _com = w::CoInitializeEx(co::COINIT::APARTMENTTHREADED | co::COINIT::DISABLE_OLE1DDE)
            .map_err(|e| format!("couldn't initialize Office automation: {e}"))?;
        let is_word = source.to_ascii_lowercase().ends_with(".doc") || source.to_ascii_lowercase().ends_with(".docx");
        if is_word { convert_word(source, target) } else { convert_powerpoint(source, target) }
    }

    fn application(prog_id: &str) -> Result<w::IDispatch, String> {
        let clsid = w::CLSIDFromProgID(prog_id).map_err(|e| format!("{prog_id} is not installed or registered: {e}"))?;
        w::CoCreateInstance::<w::IDispatch>(&clsid, None::<&w::IUnknown>, co::CLSCTX::LOCAL_SERVER)
            .map_err(|e| format!("couldn't start {prog_id}: {e}"))
    }

    fn convert_word(source: &str, target: &str) -> Result<(), String> {
        let app = application("Word.Application")?;
        let hidden = w::Variant::Bool(false);
        let no_alerts = w::Variant::I4(0);
        let no_macros = w::Variant::I4(3);
        let result = (|| {
            app.invoke_put("Visible", &hidden).map_err(|e| format!("couldn't hide Word: {e}"))?;
            app.invoke_put("DisplayAlerts", &no_alerts).map_err(|e| format!("couldn't suppress Word alerts: {e}"))?;
            app.invoke_put("AutomationSecurity", &no_macros).map_err(|e| format!("couldn't disable Office macros: {e}"))?;
            let documents = dispatch_property(&app, "Documents", "couldn't access Word documents")?;
            let path = w::Variant::from_str(source);
            let confirm = w::Variant::Bool(false);
            let read_only = w::Variant::Bool(true);
            let recent = w::Variant::Bool(false);
            let document = documents
                .invoke_method("Open", &[&path, &confirm, &read_only, &recent])
                .map_err(|e| format!("Word couldn't open the document: {e}"))?
                .into_dispatch("Word returned an invalid document object")?;
            let out = w::Variant::from_str(target);
            let pdf = w::Variant::I4(17); // wdExportFormatPDF
            let exported = document.invoke_method("ExportAsFixedFormat", &[&out, &pdf]).map_err(|e| format!("Word couldn't export PDF: {e}"));
            let dont_save = w::Variant::I4(0); // wdDoNotSaveChanges
            let _ = document.invoke_method("Close", &[&dont_save]);
            exported.map(|_| ())
        })();
        let _ = app.invoke_method("Quit", &[]);
        result
    }

    fn convert_powerpoint(source: &str, target: &str) -> Result<(), String> {
        let app = application("PowerPoint.Application")?;
        let no_alerts = w::Variant::I4(1); // ppAlertsNone
        let no_macros = w::Variant::I4(3);
        let result = (|| {
            app.invoke_put("DisplayAlerts", &no_alerts).map_err(|e| format!("couldn't suppress PowerPoint alerts: {e}"))?;
            app.invoke_put("AutomationSecurity", &no_macros).map_err(|e| format!("couldn't disable Office macros: {e}"))?;
            let presentations = dispatch_property(&app, "Presentations", "couldn't access PowerPoint presentations")?;
            let path = w::Variant::from_str(source);
            let read_only = w::Variant::Bool(true);
            let untitled = w::Variant::Bool(false);
            let with_window = w::Variant::Bool(false);
            let presentation = presentations
                .invoke_method("Open", &[&path, &read_only, &untitled, &with_window])
                .map_err(|e| format!("PowerPoint couldn't open the presentation: {e}"))?
                .into_dispatch("PowerPoint returned an invalid presentation object")?;
            let out = w::Variant::from_str(target);
            let pdf = w::Variant::I4(2); // ppFixedFormatTypePDF
            let exported =
                presentation.invoke_method("ExportAsFixedFormat", &[&out, &pdf]).map_err(|e| format!("PowerPoint couldn't export PDF: {e}"));
            let _ = presentation.invoke_method("Close", &[]);
            exported.map(|_| ())
        })();
        let _ = app.invoke_method("Quit", &[]);
        result
    }

    fn dispatch_property(app: &w::IDispatch, name: &str, error: &str) -> Result<w::IDispatch, String> {
        app.invoke_get(name, &[]).map_err(|e| format!("{error}: {e}"))?.into_dispatch(error)
    }

    trait IntoDispatch {
        fn into_dispatch(self, error: &str) -> Result<w::IDispatch, String>;
    }

    impl IntoDispatch for w::Variant {
        fn into_dispatch(self, error: &str) -> Result<w::IDispatch, String> {
            match self {
                w::Variant::Dispatch(dispatch) => Ok(dispatch),
                _ => Err(error.to_string()),
            }
        }
    }
}

fn wait_bounded(child: &mut Child) -> Result<std::process::ExitStatus, String> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("couldn't check the Office conversion process: {e}"));
            }
            Ok(Some(status)) => return Ok(status),
            Ok(None) if started.elapsed() >= TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Office conversion exceeded the 90-second time limit".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn libreoffice_executable() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let names = ["soffice.exe", "soffice"];
    #[cfg(not(target_os = "windows"))]
    let names = ["soffice", "libreoffice"];
    let path = std::env::var_os("PATH").unwrap_or_default();
    for directory in std::env::split_paths(&path) {
        for name in names {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    #[cfg(target_os = "windows")]
    for base in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
        if let Some(root) = std::env::var_os(base).filter(|v| !v.is_empty()) {
            let root = PathBuf::from(root);
            let candidate = if base == "LOCALAPPDATA" {
                root.join("Programs").join("LibreOffice").join("program").join("soffice.exe")
            } else {
                root.join("LibreOffice").join("program").join("soffice.exe")
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let candidate = PathBuf::from("/Applications/LibreOffice.app/Contents/MacOS/soffice");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(any(not(target_os = "windows"), test))]
fn file_url(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    let path = if cfg!(windows) && !path.starts_with('/') { format!("/{path}") } else { path };
    let mut encoded = String::with_capacity(path.len() + 8);
    encoded.push_str("file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}

struct TempWorkspace {
    path: PathBuf,
}

impl TempWorkspace {
    fn create() -> Result<Self, String> {
        let root = std::env::temp_dir();
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| format!("system clock is invalid: {e}"))?.as_nanos();
        for attempt in 0_u32..32 {
            let path = root.join(format!("pdfcraft-office-{}-{nonce}-{attempt}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("couldn't create a temporary Office conversion folder: {e}")),
            }
        }
        Err("couldn't reserve a unique temporary Office conversion folder".into())
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::{file_url, is_office_document, office_to_pdf};
    use std::path::Path;

    #[test]
    fn office_extensions_are_case_insensitive_and_bounded() {
        for name in ["report.doc", "report.DOCX", "slides.ppt", "slides.PPTX"] {
            assert!(is_office_document(name), "{name}");
        }
        for name in ["report.pdf", "report.docm", "report.docx.exe", "extensionless"] {
            assert!(!is_office_document(name), "{name}");
        }
    }

    #[test]
    fn office_converter_rejects_wrong_types_and_empty_data_before_launch() {
        assert!(office_to_pdf("document.pdf", b"data").is_err());
        assert!(office_to_pdf("document.docx", b"").is_err());
    }

    #[test]
    fn profile_file_urls_escape_spaces_and_keep_path_separators() {
        let url = file_url(Path::new("C:/Users/Test User/AppData/Local/pdfcraft"));
        assert!(url.starts_with("file://"));
        assert!(url.contains("%20"));
        assert!(url.contains("/AppData/Local/"));
    }
}
