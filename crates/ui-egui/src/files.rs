//! Multi-document page operations: Combine Files, Insert Pages from File, Extract Pages, Split.
//!
//! Files are picked asynchronously and used on a later frame: desktop builds through `pickers`
//! (a blocking picker crashes the app on macOS), browsers through `requests`.

use std::sync::Arc;

use pdfcraft_engine::{DocId, Edit, SplitBy};

use crate::PdfCraftApp;

/// Why files were picked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilePurpose {
    TextFont {
        page: usize,
    },
    Combine,
    InsertPages,
    ReplacePages,
    /// Scan & OCR ▸ Recognize text in multiple files.
    Ocr,
    /// Create a PDF ▸ Multiple files (PDFs, images and text).
    CreateMultiple,
    /// Create one PDF per selected non-PDF file.
    CreateMultipleSeparate,
    /// Find text and redact ▸ Import list: a text file with one word or phrase per line.
    RedactWords,
}

/// Discover installed document fonts off the GUI thread, only when the picker is opened.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn installed_font_files(ctx: &egui::Context) -> Option<Vec<pdfcraft_engine::text_fonts::FontFile>> {
    use std::sync::{Mutex, OnceLock};
    type Files = Arc<Mutex<Option<Vec<pdfcraft_engine::text_fonts::FontFile>>>>;
    static CACHE: OnceLock<Files> = OnceLock::new();
    let files = CACHE.get_or_init(|| {
        let files: Files = Arc::new(Mutex::new(None));
        let result = files.clone();
        let ctx = ctx.clone();
        if std::thread::Builder::new()
            .name("document-font-discovery".into())
            .spawn(move || {
                let fonts = pdfcraft_engine::text_fonts::discover(&pdfcraft_engine::text_fonts::installed_directories());
                if let Ok(mut result) = result.lock() {
                    *result = Some(fonts);
                }
                ctx.request_repaint();
            })
            .is_err()
            && let Ok(mut result) = files.lock()
        {
            *result = Some(Vec::new());
        }
        files
    });
    files.lock().ok().and_then(|f| f.clone())
}

/// The picker for `purpose`: PDFs, and for Create and Insert also what they convert.
fn files_picker(purpose: FilePurpose) -> rfd::AsyncFileDialog {
    let dialog = rfd::AsyncFileDialog::new();
    if matches!(purpose, FilePurpose::TextFont { .. }) {
        return dialog.add_filter("TrueType / OpenType font", &["ttf", "otf"]);
    }
    if purpose == FilePurpose::RedactWords {
        return dialog.add_filter(tl!("Text"), &["txt"]);
    }
    if !matches!(purpose, FilePurpose::CreateMultiple | FilePurpose::CreateMultipleSeparate | FilePurpose::InsertPages | FilePurpose::Combine) {
        return dialog.add_filter("PDF", &["pdf"]);
    }
    let all: Vec<&str> = std::iter::once("pdf").chain(pdfcraft_engine::CONVERTIBLE).chain(crate::create_ui::OFFICE_CONVERTIBLE).collect();
    dialog.add_filter(tl!("PDF, images and text"), &all).add_filter("PDF", &["pdf"])
}

/// The Replace Pages dialog: the chosen file and the ranges (1-based, inclusive).
#[derive(Clone, Debug, PartialEq)]
pub struct ReplaceDraft {
    pub name: String,
    pub bytes: Arc<Vec<u8>>,
    pub src_pages: usize,
    pub from: usize,
    pub to: usize,
    pub src_from: usize,
    /// The selection when the dialog opened (0-based). OK replaces these pages until the
    /// original range is edited, so a gap between selected pages is left alone.
    pub pages: Vec<usize>,
    /// The user changed the original from/to range, so OK uses that span.
    pub range_edited: bool,
    /// The document whose pages are replaced, as it was when the dialog opened: OK replaces
    /// nothing if another document is active by then (a file opened meanwhile), or it changed.
    pub(crate) target: Option<PickTarget>,
}

impl FilePurpose {
    /// Insert and Replace edit the active document, at its selection.
    pub(crate) fn edits_active_document(self) -> bool {
        matches!(self, Self::InsertPages | Self::ReplacePages | Self::TextFont { .. })
    }
}

/// The document a pick edits, as it was when the picker opened. Page positions captured then
/// are only valid while it is unchanged, and the pick acts on the active document; so a pick
/// that arrives after the document was edited, or stopped being the active one, is refused
/// (see [`PdfCraftApp::still_pick_target`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PickTarget {
    doc: DocId,
    generation: Option<u64>,
}

/// A file read for an import, with any Office conversion already completed off the UI thread.
#[derive(Debug)]
pub(crate) struct PreparedFile {
    pub name: String,
    pub bytes: Vec<u8>,
    pub modified: Option<std::time::SystemTime>,
    pub converted: Option<Result<Arc<Vec<u8>>, String>>,
}

impl PreparedFile {
    pub(crate) fn unconverted(name: String, bytes: Vec<u8>) -> Self {
        Self { name, bytes, modified: None, converted: None }
    }
}

pub(crate) type FileResult = Result<PreparedFile, (String, String)>;

/// Files picked asynchronously, waiting for the next frame (see [`PdfCraftApp::file_request`]).
#[derive(Debug)]
pub struct FileRequest {
    purpose: FilePurpose,
    target: Option<PickTarget>,
    files: Vec<FileResult>,
    output_dir: Option<std::path::PathBuf>,
    create_multiple: Option<Result<crate::create_multiple_ui::CreateMultipleOutput, crate::create_multiple_ui::CreateMultipleFailure>>,
}

/// Files picked asynchronously (web), waiting to be used.
pub type Requests = Arc<std::sync::Mutex<Vec<FileRequest>>>;

/// A file input for the native background reader.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) enum FileInput {
    Path(std::path::PathBuf),
    Bytes(String, Vec<u8>),
}

#[cfg(not(target_arch = "wasm32"))]
impl FileInput {
    pub(crate) fn name(&self) -> String {
        match self {
            FileInput::Path(path) => path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file.pdf".into()),
            FileInput::Bytes(name, _) => name.clone(),
        }
    }
}

/// A job queued for the native background file reader and Office converter.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct FileJob {
    pub inputs: Vec<FileInput>,
    pub request: FileRequest,
    pub requests: Requests,
    pub status: Arc<std::sync::Mutex<Option<String>>>,
    pub ctx: Option<egui::Context>,
}

#[cfg(not(target_arch = "wasm32"))]
fn prepare_file_input(input: FileInput) -> FileResult {
    prepare_file_input_with(input, |name, bytes| pdfcraft_platform::office_to_pdf(name, bytes).map(Arc::new))
}

#[cfg(not(target_arch = "wasm32"))]
fn prepare_file_input_with(input: FileInput, office_convert: impl FnOnce(&str, &[u8]) -> Result<Arc<Vec<u8>>, String>) -> FileResult {
    let (name, bytes, modified) = match input {
        FileInput::Path(path) => {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file.pdf".into());
            let bytes = std::fs::read(&path).map_err(|error| (name.clone(), error.to_string()))?;
            let modified = std::fs::metadata(&path).and_then(|metadata| metadata.modified()).ok();
            (name, bytes, modified)
        }
        FileInput::Bytes(name, bytes) => (name, bytes, None),
    };
    let is_pdf = pdfcraft_engine::source_kind(&name, &bytes) == Some(pdfcraft_engine::SourceKind::Pdf);
    let converted = if !is_pdf && pdfcraft_platform::is_office_document(&name) {
        let conversion = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| office_convert(&name, &bytes)))
            .unwrap_or_else(|_| Err("Office conversion failed unexpectedly in the background worker".into()));
        Some(conversion)
    } else {
        None
    };
    Ok(PreparedFile { name, bytes, modified, converted })
}

/// Settings for the Split dialog.
#[derive(Clone, Debug, PartialEq)]
pub struct SplitDraft {
    /// Pages per file.
    pub every: usize,
    pub mode: SplitMode,
    /// The largest part, in megabytes (File size mode).
    pub size_mb: f64,
}

impl Default for SplitDraft {
    fn default() -> Self {
        SplitDraft { every: 1, mode: SplitMode::Pages, size_mb: 2.0 }
    }
}

/// Acrobat's Split by: number of pages, file size, top-level bookmarks (and PdfCraft's
/// before-selected-pages).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitMode {
    Pages,
    Selection,
    Size,
    Bookmarks,
}

/// What Split does once confirmed.
#[derive(Clone, Debug, PartialEq)]
pub enum SplitPlan {
    By(SplitBy),
    Size(usize),
    Bookmarks,
}

/// Organize ▸ Extract options.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtractDraft {
    /// Each page as its own file.
    pub separate: bool,
    /// Delete the pages after extracting them.
    pub delete: bool,
    /// The name the extracted pages are saved under, without `.pdf` (#737). The dialog fills in
    /// the document's name when it opens; empty means that name too. As separate files each
    /// page is `<name> (page N).pdf`, and a single page the user renamed is `<name>.pdf`.
    pub name: String,
}

/// `name` made safe as a file name on every platform: no path separators, reserved or control
/// characters, no trailing dots or spaces (Windows drops them), no `.pdf` (added back on
/// writing), at most 200 characters. Empty when nothing usable is left.
pub(crate) fn file_stem_from_user(name: &str) -> String {
    let name = name.trim();
    // Any case of ".pdf": it's added back on writing.
    let name =
        name.len().checked_sub(4).and_then(|i| name.get(i..).filter(|ext| ext.eq_ignore_ascii_case(".pdf")).and(name.get(..i))).unwrap_or(name);
    let cleaned: String =
        name.chars().map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c }).collect();
    // At most 200 bytes, so " (page N).pdf" still fits the usual 255-byte file name limit.
    let mut end = cleaned.len().min(200);
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    let cleaned = cleaned.get(..end).unwrap_or_default();
    // Trimming trailing dots also leaves "." and ".." empty.
    let stem = cleaned.trim().trim_end_matches(['.', ' ']).trim_start();
    // Windows reserves these device names whatever the extension: "NUL.pdf" writes nowhere.
    let device = stem.split('.').next().unwrap_or_default().to_ascii_uppercase();
    let reserved = matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (device.len() == 4
            && (device.starts_with("COM") || device.starts_with("LPT"))
            && device.as_bytes().get(3).is_some_and(|d| (b'1'..=b'9').contains(d)));
    if reserved { format!("_{stem}") } else { stem.to_string() }
}

/// A plain file name for a PDF-supplied attachment name: the Save suggestion, and the tab name
/// (which later names files written from that tab). It never changes the stored name or a
/// destination the user selected.
pub(crate) fn attachment_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    let name = cleaned.trim_start().trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    if name.is_empty() {
        return "attachment".into();
    }
    let device = name.split('.').next().unwrap_or_default().trim_end().to_ascii_uppercase();
    let reserved = matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || device
            .strip_prefix("COM")
            .or_else(|| device.strip_prefix("LPT"))
            .is_some_and(|number| matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"));
    if reserved { format!("_{name}") } else { name.to_string() }
}

/// Whether `name` names a file directly inside a folder: not empty, `.` or `..`, with no `/` or
/// `\\`, and no drive (`C:`). On Windows any `:` is refused, which also rules out alternate data
/// streams.
pub(crate) fn is_plain_file_name(name: &str) -> bool {
    let drive = matches!(name.as_bytes(), [letter, b':', ..] if letter.is_ascii_alphabetic());
    !(name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) || drive || (cfg!(windows) && name.contains(':')))
}

/// Pages ▸ Rotate Pages.
#[derive(Clone, Debug, PartialEq)]
pub struct RotateDraft {
    /// Degrees clockwise: 90, 180 or 270.
    pub degrees: i64,
    /// 0 all pages, 1 the selection, 2 the range below.
    pub which: u8,
    pub from: usize,
    pub to: usize,
    pub parity: pdfcraft_engine::PageParity,
    pub orientation: pdfcraft_engine::PageOrientation,
}

impl Default for RotateDraft {
    fn default() -> Self {
        RotateDraft { degrees: 90, which: 0, from: 1, to: 1, parity: Default::default(), orientation: Default::default() }
    }
}

/// A picked file's name and bytes, or its name and why it can't be read.
#[cfg(not(target_arch = "wasm32"))]
fn read_named(path: &std::path::Path) -> Result<(String, Vec<u8>), (String, String)> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file.pdf".into());
    std::fs::read(path).map(|bytes| (name.clone(), bytes)).map_err(|error| (name, error.to_string()))
}

impl PdfCraftApp {
    /// Add paths received from a file-manager Combine command to the existing Combine tab.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn add_paths_to_combine(&mut self, paths: &[String]) {
        let paths = paths.iter().map(|path| std::path::PathBuf::from(path.as_str())).collect::<Vec<_>>();
        self.use_paths(FilePurpose::Combine, &paths);
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn submit_file_job(&mut self, mut job: FileJob) {
        if let Some(sender) = &self.file_import_worker {
            match sender.send(job) {
                Ok(()) => return,
                Err(std::sync::mpsc::SendError(returned)) => {
                    job = returned;
                    self.file_import_worker = None;
                }
            }
        }
        let (tx, rx) = std::sync::mpsc::channel::<FileJob>();
        let spawned = std::thread::Builder::new().name("pdfcraft-file-import".into()).spawn(move || {
            while let Ok(job) = rx.recv() {
                let mut request = job.request;
                if request.purpose == FilePurpose::CreateMultiple {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        crate::create_multiple_ui::build_create_multiple_output(
                            &pdfcraft_engine::Session::new(),
                            job.inputs.into_iter().map(|input| {
                                if let Ok(mut s) = job.status.lock() {
                                    *s = Some(input.name());
                                }
                                if let Some(ctx) = &job.ctx {
                                    ctx.request_repaint();
                                }
                                prepare_file_input(input)
                            }),
                        )
                    }))
                    .unwrap_or_else(|_| {
                        Err(crate::create_multiple_ui::CreateMultipleFailure::Build(
                            "file preparation failed unexpectedly in the background worker".into(),
                        ))
                    });
                    request.create_multiple = Some(result);
                } else if request.purpose == FilePurpose::Combine {
                    let mut files = Vec::with_capacity(job.inputs.len());
                    for input in job.inputs {
                        if let Ok(mut s) = job.status.lock() {
                            *s = Some(input.name());
                        }
                        if let Some(ctx) = &job.ctx {
                            ctx.request_repaint();
                        }
                        let result = prepare_file_input(input);
                        let failed_to_read = result.is_err();
                        files.push(result);
                        if failed_to_read {
                            break;
                        }
                    }
                    request.files = files;
                } else {
                    let mut files = Vec::with_capacity(job.inputs.len());
                    for input in job.inputs {
                        if let Ok(mut s) = job.status.lock() {
                            *s = Some(input.name());
                        }
                        if let Some(ctx) = &job.ctx {
                            ctx.request_repaint();
                        }
                        files.push(prepare_file_input(input));
                    }
                    request.files = files;
                }
                if let Ok(mut s) = job.status.lock() {
                    *s = None;
                }
                if let Ok(mut queue) = job.requests.lock() {
                    queue.push(request);
                    if let Some(ctx) = job.ctx {
                        ctx.request_repaint();
                    }
                }
            }
        });
        match spawned {
            Ok(_) => {
                if let Err(e) = tx.send(job) {
                    self.notify_fmt("Couldn't start preparing files: {e}", &[("e", &e.to_string())]);
                } else {
                    self.file_import_worker = Some(tx);
                }
            }
            Err(e) => {
                self.notify_fmt("Couldn't start preparing files: {e}", &[("e", &e.to_string())]);
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn queue_import(&mut self, purpose: FilePurpose, inputs: Vec<FileInput>, output_dir: Option<std::path::PathBuf>) {
        let requests = self.requests.clone();
        let status = self.file_import_name.clone();
        let ctx = self.ctx.clone();
        let mut request = self.file_request(purpose, Vec::new());
        request.output_dir = output_dir.clone();
        self.submit_file_job(FileJob { inputs, request, requests, status, ctx });
    }

    /// Ask for files to add to the Combine files list (its Add files… button).
    pub fn combine_dialog(&mut self) {
        self.pick_files(FilePurpose::Combine, true);
    }

    /// Create a PDF ▸ Multiple files: ask for the files to convert.
    pub fn create_multiple_dialog(&mut self) {
        self.pick_files(FilePurpose::CreateMultiple, true);
    }

    /// Create a PDF ▸ Separate PDFs: pick source files, then a destination folder.
    pub fn create_multiple_separate_dialog(&mut self) {
        self.pick_files(FilePurpose::CreateMultipleSeparate, true);
    }

    /// Scan & OCR ▸ Recognize text ▸ In multiple files: ask for the PDFs.
    pub fn ocr_files_dialog(&mut self) {
        self.pick_files(FilePurpose::Ocr, true);
    }

    /// Ask for files whose pages to insert after the selection (Organize ▸ Insert from file).
    pub fn insert_from_file_dialog(&mut self) {
        self.insert_from_file_at(None);
    }

    /// Ask for files to insert at grid gap `at` (0 = before the first page), or after the
    /// selection.
    pub(crate) fn insert_from_file_at(&mut self, at: Option<usize>) {
        let Some(i) = self.active else {
            self.notify_tr("Open a document first");
            return;
        };
        if let Some(v) = self.views.get_mut(i) {
            v.insert_at = at;
        }
        self.pick_files(FilePurpose::InsertPages, true);
    }

    pub(crate) fn pick_files(&mut self, purpose: FilePurpose, multiple: bool) {
        #[cfg(not(target_arch = "wasm32"))]
        self.pick(crate::pickers::PickFor::Files(purpose), files_picker(purpose), multiple);
        #[cfg(target_arch = "wasm32")]
        {
            let requests = self.requests.clone();
            let ctx = self.ctx.clone();
            // The document to insert into is the one active now, not whichever is active when
            // the browser has finished reading the file (#167).
            let request = self.file_request(purpose, Vec::new());
            wasm_bindgen_futures::spawn_local(async move {
                let dialog = files_picker(purpose);
                let handles = if multiple { dialog.pick_files().await.unwrap_or_default() } else { dialog.pick_file().await.into_iter().collect() };
                let mut files = Vec::new();
                for h in handles {
                    files.push(Ok(PreparedFile::unconverted(h.file_name(), h.read().await)));
                }
                if !files.is_empty()
                    && let Ok(mut q) = requests.lock()
                {
                    q.push(FileRequest { files, ..request });
                }
                // The read may finish while the app is idle: wake it to use the files.
                if let Some(ctx) = ctx {
                    ctx.request_repaint();
                }
            });
        }
    }

    /// Read the picked files and use them.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn use_paths(&mut self, purpose: FilePurpose, paths: &[std::path::PathBuf]) {
        if paths.is_empty() {
            return;
        }
        let has_office = paths
            .iter()
            .any(|path| path.file_name().map(|name| name.to_string_lossy()).is_some_and(|name| pdfcraft_platform::is_office_document(&name)));
        if has_office
            && matches!(purpose, FilePurpose::Combine | FilePurpose::CreateMultiple | FilePurpose::CreateMultipleSeparate | FilePurpose::InsertPages)
        {
            let inputs = paths.iter().cloned().map(FileInput::Path).collect::<Vec<_>>();
            if purpose != FilePurpose::CreateMultipleSeparate {
                self.queue_import(purpose, inputs, None);
                return;
            }
            let queue = move |app: &mut Self, dir: std::path::PathBuf| app.queue_import(FilePurpose::CreateMultipleSeparate, inputs, Some(dir));
            match self.export_dir_override.clone() {
                Some(dir) => queue(self, std::path::PathBuf::from(dir)),
                None => {
                    self.ask_one(
                        crate::pickers::Ask::Folder(rfd::AsyncFileDialog::new().set_title(tl!("Choose a folder for the exported files"))),
                        None,
                        queue,
                    );
                }
            }
            return;
        }
        match purpose {
            FilePurpose::CreateMultiple => {
                self.stage_create_multiple(paths.iter().map(|path| read_named(path)));
                return;
            }
            FilePurpose::CreateMultipleSeparate => {
                let paths = paths.to_vec();
                let convert = move |app: &mut Self, dir: std::path::PathBuf| {
                    app.stage_create_separate(paths.iter().map(|path| read_named(path)), Some(dir));
                };
                match self.export_dir_override.clone() {
                    Some(dir) => convert(self, std::path::PathBuf::from(dir)),
                    None => {
                        self.ask_one(
                            crate::pickers::Ask::Folder(rfd::AsyncFileDialog::new().set_title(tl!("Choose a folder for the exported files"))),
                            None,
                            convert,
                        );
                    }
                }
                return;
            }
            FilePurpose::InsertPages => {
                self.insert_each(paths.iter().map(|path| read_named(path)));
                return;
            }
            _ => {}
        }
        let mut files = Vec::new();
        let mut modified = Vec::new();
        for p in paths {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file.pdf".into());
            if purpose == FilePurpose::RedactWords && std::fs::metadata(p).is_ok_and(|m| m.len() > crate::redact_ui::MAX_WORD_LIST_BYTES as u64) {
                self.notify_fmt("{name} is too large for a word list (at most 1 MB).", &[("name", &name)]);
                return;
            }
            let read = if matches!(purpose, FilePurpose::TextFont { .. }) {
                use std::io::Read;
                std::fs::File::open(p).and_then(|f| {
                    let mut bytes = Vec::new();
                    f.take((16 << 20) + 1).read_to_end(&mut bytes)?;
                    Ok(bytes)
                })
            } else {
                std::fs::read(p)
            };
            match read {
                Ok(b) => files.push((name, b)),
                Err(e) => {
                    self.notify_fmt("Couldn't read {name}: {e}", &[("name", &name), ("e", &e.to_string())]);
                    return;
                }
            }
            modified.push(std::fs::metadata(p).and_then(|m| m.modified()).ok());
        }
        if files.is_empty() {
            return;
        }
        if purpose == FilePurpose::Combine {
            let incoming = files
                .into_iter()
                .zip(modified)
                .map(|((name, bytes), modified)| crate::combine_ui::Incoming {
                    name,
                    bytes: std::sync::Arc::new(bytes),
                    modified,
                    note: None,
                    open_doc: None,
                    converted: None,
                })
                .collect();
            self.stage_combine_with(incoming);
            return;
        }
        self.use_files(purpose, files);
    }

    /// A file dropped on the Combine files tab: added to its list.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn drop_into_combine(&mut self, f: egui::DroppedFileHandle, _ctx: &egui::Context) {
        // A folder: the PDFs in it and in the folders inside it.
        if f.path().is_absolute() && f.path().is_dir() {
            self.add_folder_to_combine(f.path(), true);
            return;
        }
        if f.path().is_absolute() {
            self.use_paths(FilePurpose::Combine, &[f.path().to_path_buf()]);
            return;
        }
        let name = f.path().file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "dropped.pdf".into());
        match f.bytes() {
            Ok(bytes) => self.use_files(FilePurpose::Combine, vec![(name, bytes)]),
            Err(e) => self.notify_fmt("Couldn't read {name}: {e}", &[("name", &name), ("e", &e.to_string())]),
        }
    }

    /// Browsers read dropped files asynchronously; they join the Combine files list next frame.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn drop_into_combine(&mut self, f: egui::DroppedFileHandle, ctx: &egui::Context) {
        let requests = self.requests.clone();
        let request = self.file_request(FilePurpose::Combine, Vec::new());
        let ctx = ctx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let name = f.path().file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "dropped.pdf".into());
            if let Ok(bytes) = f.bytes_async().await
                && let Ok(mut q) = requests.lock()
            {
                q.push(FileRequest { files: vec![Ok(PreparedFile::unconverted(name, bytes))], ..request });
                ctx.request_repaint();
            }
        });
    }

    /// A request to use `files` for `purpose`, bound to the document it will edit: the active
    /// one now, for Insert and Replace. Browsers make it when the picker opens and queue it on
    /// `requests` once the files are read; it is used on a later frame.
    pub fn file_request(&self, purpose: FilePurpose, files: Vec<(String, Vec<u8>)>) -> FileRequest {
        let target = if purpose.edits_active_document() { self.pick_target(self.active_ids().map(|(_, id)| id)) } else { None };
        FileRequest {
            purpose,
            target,
            files: files.into_iter().map(|(name, bytes)| Ok(PreparedFile::unconverted(name, bytes))).collect(),
            output_dir: None,
            create_multiple: None,
        }
    }

    /// `doc` as it is now, for a pick that will edit it.
    pub(crate) fn pick_target(&self, doc: Option<DocId>) -> Option<PickTarget> {
        doc.map(|doc| PickTarget { doc, generation: self.session.get(doc).map(|d| d.edit_generation()) })
    }

    /// Whether a pick for `target` may still be used: the document is still the active one and
    /// unedited. Tells the user when not.
    pub(crate) fn still_pick_target(&mut self, target: Option<PickTarget>) -> bool {
        let Some(t) = target else { return true };
        let active = self.active_ids().map(|(_, id)| id);
        let generation = self.session.get(t.doc).map(|d| d.edit_generation());
        if active == Some(t.doc) && generation == t.generation {
            return true;
        }
        self.notify_tr("The document changed while you were choosing a file, so nothing was changed.");
        false
    }

    /// Handle files picked asynchronously.
    pub(crate) fn process_file_requests(&mut self) {
        let pending: Vec<_> = self.requests.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
        for FileRequest { purpose, target, files, output_dir, create_multiple, .. } in pending {
            if !self.still_pick_target(target) {
                continue;
            }
            // Last-resort guard (AGENTS.md §4), as for desktop picks and commands.
            let use_request = || match purpose {
                FilePurpose::Combine => {
                    if let Some(Err((name, error))) = files.iter().find(|file| file.is_err()) {
                        self.notify_fmt("Couldn't read {name}: {e}", &[("name", name), ("e", error)]);
                        return;
                    }
                    let incoming = files
                        .into_iter()
                        .filter_map(Result::ok)
                        .map(|file| crate::combine_ui::Incoming {
                            name: file.name,
                            bytes: Arc::new(file.bytes),
                            modified: file.modified,
                            note: None,
                            open_doc: None,
                            converted: file.converted,
                        })
                        .collect();
                    self.stage_combine_with(incoming);
                }
                FilePurpose::CreateMultiple => match create_multiple {
                    Some(result) => self.finish_create_multiple(result),
                    None => self.stage_create_multiple_prepared(files),
                },
                FilePurpose::CreateMultipleSeparate => self.stage_create_separate_prepared(files, output_dir),
                FilePurpose::InsertPages => self.insert_prepared(files),
                _ => self.use_files(purpose, files.into_iter().filter_map(Result::ok).map(|file| (file.name, file.bytes)).collect()),
            };
            if let Err(m) = pdfcraft_engine::guard(use_request) {
                self.notify_fmt("That didn't work: an internal error stopped it ({m}).", &[("m", m.as_str())]);
            }
        }
    }

    /// Use picked files (also the entry point for tests and automation).
    pub fn use_files(&mut self, purpose: FilePurpose, files: Vec<(String, Vec<u8>)>) {
        #[cfg(not(target_arch = "wasm32"))]
        if matches!(purpose, FilePurpose::Combine | FilePurpose::CreateMultiple | FilePurpose::CreateMultipleSeparate | FilePurpose::InsertPages)
            && files.iter().any(|(name, _)| pdfcraft_platform::is_office_document(name))
        {
            let inputs = files.into_iter().map(|(name, bytes)| FileInput::Bytes(name, bytes)).collect();
            if purpose != FilePurpose::CreateMultipleSeparate {
                self.queue_import(purpose, inputs, None);
                return;
            }
            let queue = move |app: &mut Self, dir: std::path::PathBuf| {
                app.queue_import(FilePurpose::CreateMultipleSeparate, inputs, Some(dir));
            };
            match self.export_dir_override.clone() {
                Some(dir) => queue(self, std::path::PathBuf::from(dir)),
                None => {
                    self.ask_one(
                        crate::pickers::Ask::Folder(rfd::AsyncFileDialog::new().set_title(tl!("Choose a folder for the exported files"))),
                        None,
                        queue,
                    );
                }
            }
            return;
        }
        match purpose {
            FilePurpose::TextFont { page } => {
                let Some((_, bytes)) = files.into_iter().next() else { return };
                let bytes = Arc::new(bytes);
                if !self.apply_edit(Edit::RegisterTextFont { page, bytes: bytes.clone() }) {
                    return;
                }
                let Some((index, id)) = self.active_ids() else { return };
                let Some(mut ed) = self.views[index].line_editor.clone().filter(|ed| ed.page == page) else { return };
                // The font just loaded is the one the user wants: apply it to the paragraph, as
                // choosing it from the document-font menu does, instead of only registering it (#2591).
                let resource = self.session.get(id).and_then(|doc| {
                    doc.text_editor_fonts(page).into_iter().find(|f| f.bytes == *bytes && !f.resource.starts_with("PCApproved")).map(|f| f.resource)
                });
                let ctx = self.ctx.clone();
                if resource.is_some() {
                    ed.font_resource = resource;
                    self.apply_line_format(index, ed, ctx.as_ref());
                } else if let Some(doc) = self.session.get(id)
                    && let Some(ctx) = ctx
                {
                    ed.fonts = crate::edit_text_ui::install_source_fonts(&ctx, doc, page);
                    self.views[index].line_editor = Some(ed);
                }
            }
            FilePurpose::Combine => self.stage_combine(files),
            FilePurpose::InsertPages => self.insert_files(files),
            FilePurpose::ReplacePages => {
                if let Some((name, bytes)) = files.into_iter().next() {
                    self.start_replace(name, bytes);
                }
            }
            FilePurpose::Ocr => self.ocr_files(files),
            FilePurpose::CreateMultiple => self.stage_create_multiple(files.into_iter().map(Ok)),
            FilePurpose::CreateMultipleSeparate => {
                let dir = self.export_dir_override.as_ref().map(std::path::PathBuf::from);
                self.stage_create_separate(files.into_iter().map(Ok), dir);
            }
            FilePurpose::RedactWords => {
                let Some((name, bytes)) = files.into_iter().next() else { return };
                if bytes.len() > crate::redact_ui::MAX_WORD_LIST_BYTES {
                    self.notify_fmt("{name} is too large for a word list (at most 1 MB).", &[("name", &name)]);
                    return;
                }
                self.redact_search.words = pdfcraft_engine::redact_word_list(&pdfcraft_engine::decode_text(&bytes)).join("\n");
                self.redact_search.mode = crate::redact_ui::SearchMode::Words;
            }
        }
    }

    pub fn replace_pages_dialog(&mut self) {
        if self.active.is_none() {
            self.notify_tr("Open a document first");
            return;
        }
        self.pick_files(FilePurpose::ReplacePages, false);
    }

    /// Open the Replace Pages dialog for `bytes`, replacing the selection (or the current page).
    pub fn start_replace(&mut self, name: String, bytes: Vec<u8>) {
        let Some(i) = self.active else { return };
        let bytes = Arc::new(bytes);
        let src_pages = match self.session.page_count_of(&name, &bytes) {
            Ok(n) => n,
            Err(e) => {
                self.notify_fmt("Couldn't use {name}: {e}", &[("name", &name), ("e", &e.to_string())]);
                return;
            }
        };
        let pages = self.views[i].target_pages();
        let (from, to) = (pages.first().map_or(1, |p| p + 1), pages.last().map_or(1, |p| p + 1));
        let target = self.pick_target(self.active_ids().map(|(_, id)| id));
        self.replace_draft = Some(ReplaceDraft { name, bytes, src_pages, from, to, src_from: 1, pages, range_edited: false, target });
        self.dialog = Some(crate::Dialog::ReplacePages);
    }

    /// Insert all pages of a file after the organize selection (or the current page).
    pub fn insert_pages_from(&mut self, name: &str, bytes: Vec<u8>) {
        self.insert_files(vec![(name.to_string(), bytes)]);
    }

    /// Insert the pages of `files` (PDFs, images, text), in order, at the gap a "+" in the page
    /// grid chose, or else after the selection (or the current page); then select them.
    pub fn insert_files(&mut self, files: Vec<(String, Vec<u8>)>) {
        self.insert_each(files.into_iter().map(Ok));
    }

    /// [`Self::insert_files`] for files read as their turn comes: one that can't be read is skipped
    /// and named.
    pub(crate) fn insert_each<I>(&mut self, files: I)
    where
        I: IntoIterator<Item = Result<(String, Vec<u8>), (String, String)>>,
    {
        self.insert_prepared(files.into_iter().map(|file| file.map(|(name, bytes)| PreparedFile::unconverted(name, bytes))));
    }

    pub(crate) fn insert_prepared<I>(&mut self, files: I)
    where
        I: IntoIterator<Item = FileResult>,
    {
        let Some(view) = self.active.and_then(|i| self.views.get_mut(i)) else { return };
        let (id, chosen) = (view.id, view.insert_at.take());
        let after = view.target_pages().last().map_or(0, |p| p + 1);
        let count = self.session.get(id).map_or(0, |d| d.info.pages.len());
        let start = chosen.unwrap_or(after).min(count);
        let mut at = start;
        for file in files {
            let file = match file {
                Ok(file) => file,
                Err((name, error)) => {
                    self.notify_fmt("Couldn't read {name}: {e}", &[("name", &name), ("e", &error)]);
                    continue;
                }
            };
            let name = file.name.clone();
            let converted = match file.converted {
                Some(Ok(pdf)) => Ok(pdf),
                Some(Err(error)) => Err(error),
                None if pdfcraft_engine::source_kind(&name, &file.bytes) == Some(pdfcraft_engine::SourceKind::Pdf) => Ok(Arc::new(file.bytes)),
                None => crate::create_ui::convert_input_to_pdf(&self.session, &name, &file.bytes),
            }
            .and_then(|pdf| self.session.page_count_of(&name, &pdf).map(|pages| (pages, pdf)).map_err(|e| e.to_string()));
            match converted {
                Ok((pages, bytes)) => {
                    if self.apply_edit(Edit::InsertPagesFrom {
                        name,
                        bytes,
                        pages: None,
                        at,
                        field_conflicts: pdfcraft_engine::FieldConflicts::Rename,
                    }) {
                        at = at.saturating_add(pages);
                    }
                }
                Err(error) => self.notify_fmt("Couldn't insert {name}: {error}", &[("name", &name), ("error", &error)]),
            }
        }
        if at > start
            && let Some(view) = self.active.and_then(|i| self.views.get_mut(i))
        {
            view.select_pages(&(start..at).collect::<Vec<_>>());
        }
    }

    /// Show Extract pages, its file name filled in with the active document's (#737).
    pub fn open_extract_dialog(&mut self) {
        self.extract_draft.name =
            self.active_ids().and_then(|(_, id)| self.session.get(id)).map(|d| strip_pdf(&d.name).to_string()).unwrap_or_default();
        self.dialog = Some(crate::Dialog::Extract);
    }

    /// Copy the selected pages (or the current page) into a new unsaved document tab.
    pub fn extract_selection(&mut self) {
        // What's typed in a form field is part of the document (#166).
        if !self.commit_form_typing() {
            return;
        }
        let Some((i, id)) = self.active_ids() else { return };
        let pages = self.views[i].target_pages();
        let stem = self.session.get(id).map(|d| strip_pdf(&d.name).to_string()).unwrap_or_default();
        let opts = self.extract_draft.clone();
        // The name typed in the dialog (#737), else the document's.
        let chosen = file_stem_from_user(&opts.name);
        let renamed = !chosen.is_empty() && chosen != file_stem_from_user(&stem);
        let base = if chosen.is_empty() { stem.clone() } else { chosen };
        if opts.separate {
            // Each page as its own file, in a chosen folder.
            let mut named = Vec::new();
            for &p in &pages {
                match self.session.extract(id, &[p]) {
                    // One page the user named is saved under exactly that name.
                    Ok(bytes) if renamed && pages.len() == 1 => named.push((format!("{base}.pdf"), bytes)),
                    Ok(bytes) => named.push((format!("{base} (page {}).pdf", p + 1), bytes)),
                    Err(e) => {
                        self.notify_fmt("Couldn't extract pages: {e}", &[("e", &e.to_string())]);
                        return;
                    }
                }
            }
            // Delete the pages only once their files are written, which is on a later frame when
            // the user chooses the folder, and only if they are still what was written: the
            // document must be unedited since (page numbers shift, and edits to those pages
            // aren't in the files) and still the active one (deleting edits the active document).
            let generation = self.session.get(id).map(|d| d.edit_generation());
            self.write_files_then(named, tl!("Choose a folder for the extracted pages"), move |app| {
                if !opts.delete {
                    return;
                }
                let unchanged = app.session.get(id).map(|d| d.edit_generation()) == generation;
                if unchanged && app.active_ids().map(|(_, active)| active) == Some(id) {
                    app.apply_edit(pdfcraft_engine::Edit::DeletePages { pages });
                } else {
                    app.notify_tr("The pages were extracted but not deleted, because the document changed while you were choosing a folder.");
                }
            });
            return;
        } else {
            match self.session.extract(id, &pages) {
                Ok(bytes) => {
                    let message = if pages.len() == 1 {
                        tl!("Extracted 1 page").to_string()
                    } else {
                        crate::i18n::fmt(tl!("Extracted {n} pages"), &[("n", &pages.len().to_string())])
                    };
                    // Save As offers the tab's name, so a name typed in the dialog is kept.
                    let name = if renamed { format!("{base}.pdf") } else { format!("{stem} (extract).pdf") };
                    self.open_created(&name, bytes, &message)
                }
                Err(e) => {
                    self.notify_fmt("Couldn't extract pages: {e}", &[("e", &e.to_string())]);
                    return;
                }
            }
        }
        if opts.delete {
            // Back on the original document.
            self.active = Some(i);
            self.apply_edit(pdfcraft_engine::Edit::DeletePages { pages });
        }
    }

    /// Write named files into a chosen folder (desktop) or as downloads (web).
    pub(crate) fn write_files(&mut self, named: &[(String, Arc<Vec<u8>>)], title: &str) {
        self.write_files_then(named.to_vec(), title, |_| {});
    }

    /// [`Self::write_files`], then `after` once every file is written: now, or on a later frame
    /// when the user chooses the folder. `after` never runs when writing fails or is cancelled.
    /// Every name must be a plain file name ([`is_plain_file_name`]); otherwise nothing is written.
    pub(crate) fn write_files_then(&mut self, named: Vec<(String, Arc<Vec<u8>>)>, title: &str, after: impl FnOnce(&mut Self) + Send + 'static) {
        if let Some((name, _)) = named.iter().find(|(name, _)| !is_plain_file_name(name)) {
            self.notify_fmt("Couldn't write {name}: {e}", &[("name", name), ("e", tl!("the name is not a plain file name"))]);
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let write = move |app: &mut Self, dir: std::path::PathBuf| {
                for (name, bytes) in &named {
                    if let Err(e) = crate::editing::write_atomically(&dir.join(name).to_string_lossy(), bytes) {
                        app.notify_fmt("Couldn't write {name}: {e}", &[("name", name), ("e", &e.to_string())]);
                        return;
                    }
                }
                if named.len() == 1 {
                    app.notify_fmt("Wrote 1 file to {dir}", &[("dir", &dir.display().to_string())]);
                } else {
                    app.notify_fmt("Wrote {n} files to {dir}", &[("n", &named.len().to_string()), ("dir", &dir.display().to_string())]);
                }
                after(app);
            };
            match &self.export_dir_override {
                Some(d) => write(self, std::path::PathBuf::from(d)),
                None => {
                    self.ask_one(crate::pickers::Ask::Folder(rfd::AsyncFileDialog::new().set_title(title)), None, write);
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = title;
            for (name, bytes) in &named {
                if let Err(e) = crate::editing::download(name, bytes) {
                    self.notify_fmt("Couldn't download {name}: {e}", &[("name", name), ("e", &e.to_string())]);
                    return;
                }
            }
            after(self);
        }
    }

    /// Pages ▸ Rotate Pages with the dialog's range and filters.
    pub fn rotate_with_draft(&mut self) {
        let Some((i, id)) = self.active_ids() else { return };
        let Some(doc) = self.session.get(id) else { return };
        let n = doc.info.pages.len();
        let d = self.rotate_draft.clone();
        let base: Vec<usize> = match d.which {
            1 => self.views[i].target_pages(),
            2 => (d.from.max(1) - 1..d.to.min(n)).collect(),
            _ => (0..n).collect(),
        };
        let pages = pdfcraft_engine::filter_pages(&doc.info, &base, d.parity, d.orientation);
        if pages.is_empty() {
            self.notify_tr("No pages match those choices");
            return;
        }
        self.apply_edit(pdfcraft_engine::Edit::RotatePages { pages, degrees: d.degrees });
    }

    /// Split the active document and write the parts: into a chosen folder (desktop, written on
    /// a later frame once the user has chosen it) or as downloads (web).
    pub fn split_active(&mut self, plan: &SplitPlan) {
        // What's typed in a form field is part of the document (#166).
        if !self.commit_form_typing() {
            return;
        }
        let Some((_, id)) = self.active_ids() else { return };
        let stem = self.session.get(id).map(|d| strip_pdf(&d.name).to_string()).unwrap_or_else(|| "document".into());
        let (parts, titles) = match plan {
            SplitPlan::By(by) => (self.session.split(id, by), Vec::new()),
            SplitPlan::Size(max) => (self.session.split_by_size(id, *max), Vec::new()),
            SplitPlan::Bookmarks => {
                let marks = self.session.bookmark_splits(id);
                let cuts: Vec<usize> = marks.iter().map(|m| m.0).collect();
                (self.session.split(id, &SplitBy::Before(cuts)), marks)
            }
        };
        let parts = match parts {
            Ok(p) => p,
            Err(e) => {
                self.notify_fmt("Couldn't split: {e}", &[("e", &e.to_string())]);
                return;
            }
        };
        let safe = |t: &str| t.chars().map(|c| if c.is_alphanumeric() || " -_.,()".contains(c) { c } else { '_' }).collect::<String>();
        let mut used = std::collections::HashSet::new();
        let named: Vec<(String, Arc<Vec<u8>>)> = parts
            .into_iter()
            .map(|(a, b, bytes)| {
                // Bookmark splits are named after the bookmark that starts the part.
                let title = titles.iter().find(|(p, _)| *p + 1 == a).map(|(_, t)| safe(t));
                let name = match title {
                    Some(t) => format!("{stem} - {t}.pdf"),
                    None if a == b => format!("{stem} (page {a}).pdf"),
                    None => format!("{stem} (pages {a}-{b}).pdf"),
                };
                // Equal titles (or titles that sanitize alike) get (2), (3), ... so no part overwrites another; compared case-insensitively.
                let mut unique = name.clone();
                let mut n = 1;
                while !used.insert(unique.to_lowercase()) {
                    n += 1;
                    unique = format!("{} ({n}).pdf", name.trim_end_matches(".pdf"));
                }
                (unique, bytes)
            })
            .collect();
        self.write_files(&named, "Choose a folder for the split files");
    }

    /// Summarize Comments: make the summary and open it as a new document.
    pub fn summarize_comments(&mut self) {
        let Some((_, id)) = self.active_ids() else { return };
        let stem =
            self.session.get(id).map(|d| std::path::Path::new(&d.name).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
        match self.session.summarize_comments(id, self.summary_sort) {
            Ok(bytes) => self.open_created(&format!("Summary of comments on {}.pdf", stem.unwrap_or_default()), bytes, "Created a comment summary"),
            Err(e) => self.notify_fmt("Couldn't summarize comments: {e}", &[("e", &e.to_string())]),
        }
    }

    pub(crate) fn open_created(&mut self, name: &str, bytes: Arc<Vec<u8>>, message: &str) {
        match self.session.open_new(name, bytes) {
            Ok(id) => {
                // The reading-colours preference is app-wide: the newly created document starts with it.
                self.session.set_reading_colors(id, self.reading_colors);
                let Some(doc) = self.session.get(id) else { return };
                self.views.push(crate::DocView::new(id, &doc.info, self.view_defaults));
                self.active = Some(self.views.len() - 1);
                self.notify_tr(message);
            }
            Err(e) => self.notify_fmt("Couldn't open the result: {e}", &[("e", &e.to_string())]),
        }
    }
}

pub(crate) fn strip_pdf(name: &str) -> &str {
    name.strip_suffix(".pdf").or_else(|| name.strip_suffix(".PDF")).unwrap_or(name)
}

impl PdfCraftApp {
    /// Comments ▸ Import comments / Prepare a form ▸ Import data: XFDF, FDF, XML, CSV or text.
    pub fn import_data_dialog(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let import = |app: &mut Self, path: std::path::PathBuf| match std::fs::read(&path) {
                Ok(bytes) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    app.apply_edit(pdfcraft_engine::Edit::ImportData { name, bytes: std::sync::Arc::new(bytes) });
                }
                Err(e) => app.notify_fmt("Couldn't read {name}: {e}", &[("name", &path.display().to_string()), ("e", &e.to_string())]),
            };
            match self.save_override.clone() {
                Some(p) if [".xfdf", ".fdf", ".xml", ".xdp", ".csv", ".txt"].iter().any(|e| p.ends_with(e)) => import(self, p.into()),
                Some(_) => {}
                None => {
                    let dialog = rfd::AsyncFileDialog::new()
                        .add_filter(tl!("Comment and form data").to_string(), &["xfdf", "fdf", "xml", "xdp", "csv", "txt"])
                        .set_title(tl!("Import data").to_string());
                    let target = self.active_ids().map(|(_, id)| id);
                    self.ask_one(crate::pickers::Ask::File(dialog), target, import);
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        self.notify_tr("Importing data arrives on the web with file pickers for data files");
    }

    /// Export all comments / form data: the format follows the file name's extension.
    /// Export a PDF ▸ Word, HTML or RTF: ask where (`save_override` in tests), then write.
    pub fn export_office_dialog(&mut self, format: pdfcraft_engine::compare::OfficeFormat) {
        // What's typed in a form field is part of the document (#166).
        if !self.commit_form_typing() {
            return;
        }
        let Some((_, id)) = self.active_ids() else { return };
        if !self.may_copy(id) {
            return;
        }
        let Some(doc) = self.session.get(id) else { return };
        let stem = doc.name.trim_end_matches(".pdf").trim_end_matches(".PDF").to_string();
        let ext = format.extension();
        let bytes = doc.export_office(format);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let write = move |app: &mut Self, path: std::path::PathBuf| match crate::editing::write_atomically(&path.to_string_lossy(), &bytes) {
                Ok(()) => app.notify_fmt("Exported to {path}", &[("path", &path.display().to_string())]),
                Err(e) => app.notify_fmt("Couldn't write {name}: {e}", &[("name", &path.display().to_string()), ("e", &e.to_string())]),
            };
            match self.save_override.clone() {
                Some(p) => write(self, p.into()),
                None => {
                    let dialog = rfd::AsyncFileDialog::new()
                        .set_title(tl!("Export").to_string())
                        .add_filter(ext.to_uppercase(), &[ext])
                        .set_file_name(format!("{stem}.{ext}"));
                    self.ask_one(crate::pickers::Ask::Save(dialog), None, write);
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        if let Err(e) = crate::editing::download(&format!("{stem}.{ext}"), &bytes) {
            self.notify_error(e);
        }
    }

    /// Forms ▸ Merge data files into spreadsheet: choose data files (FDF, XFDF or filled-in PDF
    /// forms), then where to save the CSV.
    pub fn merge_data_dialog(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let dialog = rfd::AsyncFileDialog::new()
                .set_title(tl!("Select data files to merge").to_string())
                .add_filter(tl!("Form data and PDF forms").to_string(), &["fdf", "xfdf", "pdf"]);
            self.ask(crate::pickers::Ask::Files(dialog), None, |app, paths| {
                let mut files = Vec::new();
                for p in paths {
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    match std::fs::read(&p) {
                        Ok(b) => files.push((name, b)),
                        Err(e) => return app.notify_fmt("Couldn't read {name}: {e}", &[("name", &name), ("e", &e.to_string())]),
                    }
                }
                if !files.is_empty() {
                    // Asks where to save: a second picker, now that the first one has closed.
                    app.merge_data_files(files);
                }
            });
        }
        #[cfg(target_arch = "wasm32")]
        self.notify_tr("Merging data files needs the desktop app");
    }

    /// Merge the given data files and save the spreadsheet (asks where; `save_override` in tests).
    pub fn merge_data_files(&mut self, files: Vec<(String, Vec<u8>)>) {
        let csv = match pdfcraft_engine::merge_data_files(&files) {
            Ok(c) => c,
            Err(e) => return self.notify_error(e),
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            let count = files.len();
            let write =
                move |app: &mut Self, path: std::path::PathBuf| match crate::editing::write_atomically(&path.to_string_lossy(), csv.as_bytes()) {
                    Ok(()) => {
                        if count == 1 {
                            app.notify_fmt("Merged 1 file into {path}", &[("path", &path.display().to_string())]);
                        } else {
                            app.notify_fmt("Merged {n} files into {path}", &[("n", &count.to_string()), ("path", &path.display().to_string())]);
                        }
                    }
                    Err(e) => app.notify_fmt("Couldn't write {name}: {e}", &[("name", &path.display().to_string()), ("e", &e.to_string())]),
                };
            match self.save_override.clone() {
                Some(p) => write(self, p.into()),
                None => {
                    let dialog = rfd::AsyncFileDialog::new()
                        .set_title(tl!("Save the spreadsheet").to_string())
                        .add_filter("CSV", &["csv"])
                        .set_file_name("report.csv");
                    self.ask_one(crate::pickers::Ask::Save(dialog), None, write);
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        let _ = crate::editing::download("report.csv", csv.as_bytes());
    }

    pub fn export_data_dialog(&mut self, comments: bool, fields: bool) {
        // What's typed in a form field is part of the document (#166).
        if !self.commit_form_typing() {
            return;
        }
        let Some((_, id)) = self.active_ids() else { return };
        let Some(doc) = self.session.get(id) else { return };
        let stem = doc.name.trim_end_matches(".pdf").trim_end_matches(".PDF").to_string();
        #[cfg(not(target_arch = "wasm32"))]
        {
            let xfa = doc.info.xfa.is_some();
            let write = move |app: &mut Self, path: std::path::PathBuf| {
                // Include what was typed while the save panel was open (#166).
                if !app.commit_typing_in(id) {
                    return;
                }
                let ext = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
                let format = pdfcraft_engine::DataFormat::from_extension(&ext).unwrap_or(pdfcraft_engine::DataFormat::Xfdf);
                match app.session.export_data(id, format, comments, fields) {
                    Ok(bytes) => match crate::editing::write_atomically(&path.to_string_lossy(), &bytes) {
                        Ok(()) => app.notify_fmt("Exported to {path}", &[("path", &path.display().to_string())]),
                        Err(e) => app.notify_fmt("Couldn't write {name}: {e}", &[("name", &path.display().to_string()), ("e", &e.to_string())]),
                    },
                    Err(e) => app.notify_error(e),
                }
            };
            match self.save_override.clone() {
                Some(p) => write(self, p.into()),
                None => {
                    let title = if comments { tl!("Export comments") } else { tl!("Export form data") };
                    let d = rfd::AsyncFileDialog::new().set_title(title).set_file_name(format!("{stem}.xfdf"));
                    let d = if comments {
                        d.add_filter("XFDF", &["xfdf"]).add_filter("FDF", &["fdf"])
                    } else if xfa {
                        // An XFA form's data, as Acrobat exports it, first.
                        d.set_file_name(format!("{stem}.xml"))
                            .add_filter("XML", &["xml"])
                            .add_filter("XDP", &["xdp"])
                            .add_filter("XFDF", &["xfdf"])
                            .add_filter("FDF", &["fdf"])
                            .add_filter("CSV", &["csv"])
                            .add_filter(tl!("Text").to_string(), &["txt"])
                    } else {
                        d.add_filter("XFDF", &["xfdf"])
                            .add_filter("FDF", &["fdf"])
                            .add_filter("XML", &["xml"])
                            .add_filter("CSV", &["csv"])
                            .add_filter(tl!("Text").to_string(), &["txt"])
                    };
                    self.ask_one(crate::pickers::Ask::Save(d), None, write);
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        match self.session.export_data(id, pdfcraft_engine::DataFormat::Xfdf, comments, fields) {
            Ok(bytes) => {
                let _ = crate::editing::download(&format!("{stem}.xfdf"), &bytes);
            }
            Err(e) => self.notify_error(e),
        }
    }
}

#[cfg(test)]
mod stem_tests {
    #[cfg(not(target_arch = "wasm32"))]
    use super::FileInput;
    use super::{FilePurpose, attachment_file_name, file_stem_from_user, is_plain_file_name};
    #[cfg(not(target_arch = "wasm32"))]
    use std::sync::Arc;

    #[test]
    fn user_file_names_stay_inside_the_folder_and_writable_everywhere() {
        assert_eq!(file_stem_from_user("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(file_stem_from_user(".."), "");
        assert_eq!(file_stem_from_user("Report.PDF"), "Report");
        // Windows device names, in any case and with any extension, get a prefix.
        assert_eq!(file_stem_from_user("nul"), "_nul");
        assert_eq!(file_stem_from_user("COM1.pdf"), "_COM1");
        assert_eq!(file_stem_from_user("lpt9.tar"), "_lpt9.tar");
        assert_eq!(file_stem_from_user("COM10"), "COM10");
        assert_eq!(file_stem_from_user("Console"), "Console");
        // At most 200 bytes, cut on a character boundary.
        let long = file_stem_from_user(&"文".repeat(300));
        assert!(long.len() <= 200 && long.chars().all(|c| c == '文'), "{}", long.len());
    }
    #[test]
    fn attachment_save_name_removes_bidi_without_hiding_the_extension() {
        assert_eq!(attachment_file_name("invoice\u{202e}fdp.exe"), "invoicefdp.exe");
        for c in [
            '\u{061c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}',
            '\u{2069}',
        ] {
            assert_eq!(attachment_file_name(&format!("a{c}b.pdf")), "ab.pdf");
        }
    }
    #[test]
    fn attachment_save_name_preserves_ordinary_unicode_and_extensions() {
        for name in ["report.pdf", "archive.tar.gz", "مرحبا.pdf", "שלום.txt", "資料.pdf", "می\u{200c}روم.txt", "👩\u{200d}💻.png", "COM10.txt"]
        {
            assert_eq!(attachment_file_name(name), name);
        }
    }
    #[test]
    fn attachment_save_name_handles_controls_paths_and_device_names() {
        assert_eq!(attachment_file_name("a\u{0}\n\u{85}b.pdf"), "ab.pdf");
        assert_eq!(attachment_file_name("../a\\b:c*?\"<>|.pdf"), ".._a_b_c______.pdf");
        for name in ["NUL.tar.gz", "con.txt", "COM1.exe", "LPT9.txt", "COM¹.txt", "LPT³.pdf"] {
            assert_eq!(attachment_file_name(name), format!("_{name}"));
        }
    }
    #[test]
    fn attachment_save_name_has_a_stable_nonempty_fallback() {
        for name in ["", " ", ".", "..", " . . ", "\u{202e}", "\u{0}\n"] {
            assert_eq!(attachment_file_name(name), "attachment");
        }
        assert_eq!(attachment_file_name(" report.pdf . \u{2003}."), "report.pdf");
        for name in ["invoice\u{202e}fdp.exe", "../a.pdf", "NUL.txt", " . . ", "report.pdf . ", "資料.pdf"] {
            let once = attachment_file_name(name);
            assert_eq!(attachment_file_name(&once), once);
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn office_bytes_are_converted_but_pdf_bytes_with_office_names_are_not() {
        let converted_pdf = Arc::new(pdfcraft_engine::Session::new().create_blank(200.0, 300.0, 1).unwrap().as_ref().clone());
        let office_called = std::cell::Cell::new(false);
        let office = super::prepare_file_input_with(FileInput::Bytes("slides.PPTX".into(), b"synthetic Office source".to_vec()), |name, bytes| {
            office_called.set(true);
            assert_eq!(name, "slides.PPTX");
            assert_eq!(bytes, b"synthetic Office source");
            Ok(converted_pdf.clone())
        })
        .unwrap();
        assert!(office_called.get());
        assert!(matches!(office.converted, Some(Ok(ref pdf)) if Arc::ptr_eq(pdf, &converted_pdf)));

        office_called.set(false);
        let source_pdf = pdfcraft_engine::Session::new().create_blank(200.0, 300.0, 1).unwrap();
        let mut pdf_bytes = b"prefix before the PDF header\n".to_vec();
        pdf_bytes.extend_from_slice(source_pdf.as_slice());
        let pdf = super::prepare_file_input_with(FileInput::Bytes("named.docx".into(), pdf_bytes), |_, _| {
            office_called.set(true);
            Err("the Office converter must not receive PDF bytes".into())
        })
        .unwrap();
        assert!(!office_called.get());
        assert!(pdf.converted.is_none());
    }

    #[test]
    fn insert_pages_uses_prepared_office_pdf_bytes() {
        let mut app = crate::PdfCraftApp::new();
        let target = app.session.create_blank(200.0, 300.0, 1).unwrap();
        app.open_bytes("target.pdf", None, target.as_ref().clone()).unwrap();
        let inserted = app.session.create_blank(200.0, 300.0, 1).unwrap();
        app.insert_prepared(vec![Ok(super::PreparedFile {
            name: "slides.pptx".into(),
            bytes: b"synthetic Office source".to_vec(),
            modified: None,
            converted: Some(Ok(inserted)),
        })]);
        let id = app.views[0].id;
        assert_eq!(app.session.get(id).unwrap().info.pages.len(), 2);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn combine_paths_are_read_on_worker_and_staged_in_selection_order() {
        let mut app = crate::PdfCraftApp::new();
        let pdf = app.session.create_blank(200.0, 300.0, 1).unwrap();
        let dir = std::env::temp_dir().join(format!("pdfcraft-file-import-{}-{}", std::process::id(), pdf.len()));
        std::fs::create_dir(&dir).unwrap();
        let first = dir.join("first.docx");
        let second = dir.join("second.docx");
        std::fs::write(&first, pdf.as_slice()).unwrap();
        std::fs::write(&second, pdf.as_slice()).unwrap();

        app.add_paths_to_combine(&[first.to_string_lossy().into_owned(), second.to_string_lossy().into_owned()]);
        assert!(app.combine_draft.is_empty(), "the caller returns without staging file reads on the frame thread");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if app.requests.lock().map(|requests| !requests.is_empty()).unwrap_or(false) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "background import did not complete");
            std::thread::yield_now();
        }
        app.process_file_requests();
        assert_eq!(app.combine_draft.iter().map(|file| file.name.as_str()).collect::<Vec<_>>(), ["first.docx", "second.docx"]);
        assert!(app.combine_draft.iter().all(|file| file.pages == 1));

        drop(app);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn create_multiple_paths_are_combined_by_the_worker_in_selection_order() {
        let mut app = crate::PdfCraftApp::new();
        let first_pdf = app.session.create_blank(200.0, 300.0, 1).unwrap();
        let second_pdf = app.session.create_blank(200.0, 300.0, 2).unwrap();
        let dir = std::env::temp_dir().join(format!("pdfcraft-create-multiple-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let first = dir.join("first.docx");
        let second = dir.join("second.docx");
        std::fs::write(&first, first_pdf.as_slice()).unwrap();
        std::fs::write(&second, second_pdf.as_slice()).unwrap();

        app.queue_import(FilePurpose::CreateMultiple, vec![FileInput::Path(first), FileInput::Path(second)], None);
        assert!(app.views.is_empty(), "conversion and combination stay off the frame thread");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if app.requests.lock().map(|requests| !requests.is_empty()).unwrap_or(false) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "background import did not complete");
            std::thread::yield_now();
        }
        app.process_file_requests();
        assert_eq!(app.views.len(), 1);
        let doc = app.session.get(app.views[0].id).unwrap();
        assert_eq!(doc.name, "Combined.pdf");
        assert_eq!(doc.info.pages.len(), 3);

        drop(app);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn write_files_refuses_names_that_leave_the_folder() {
        let dir = std::env::temp_dir().join(format!("pdfcraft-write-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join("a").join("out");
        std::fs::create_dir_all(&out).unwrap();
        let absolute = dir.join("abs.pdf").to_string_lossy().into_owned();
        let mut app = crate::PdfCraftApp::new();
        app.set_option("language", "en").unwrap();
        app.export_dir_override = Some(out.to_string_lossy().into_owned());
        for bad in ["../x.pdf", "..", ".", "", "sub/x.pdf", "sub\\x.pdf", "C:x.pdf", absolute.as_str()] {
            assert!(!is_plain_file_name(bad), "{bad}");
            // A plain name first: nothing is written when any name in the batch is refused.
            app.write_files(&[("ok.pdf".into(), std::sync::Arc::new(b"%PDF".to_vec())), (bad.into(), std::sync::Arc::new(b"%PDF".to_vec()))], "t");
            assert!(app.toast.as_ref().is_some_and(|t| t.0.contains("not a plain file name")), "{bad}");
        }
        let names: Vec<_> = walk(&dir);
        assert!(names.is_empty(), "{names:?}");
        app.write_files(&[("x.pdf".into(), std::sync::Arc::new(b"%PDF".to_vec()))], "t");
        assert_eq!(walk(&dir), vec![out.join("x.pdf")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() { out.extend(walk(&path)) } else { out.push(path) }
        }
        out
    }
}
