//! Create a PDF from multiple files. Combine opens one unsaved document in the page grid;
//! Separate writes one PDF per convertible source without opening tabs.

use std::collections::HashSet;
use std::path::PathBuf;

use pdfcraft_engine::{EditError, SourceKind};

use crate::PdfCraftApp;

fn stem(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// Why Create Multiple stopped early: a picked file couldn't be read, or the PDF couldn't be made.
enum Stop {
    Read { name: String, error: String },
    Build(EditError),
}

#[derive(Debug)]
pub(crate) enum CreateMultipleFailure {
    Read { name: String, error: String },
    Build(String),
}

#[derive(Debug)]
pub(crate) struct CreateMultipleOutput {
    pub bytes: Option<std::sync::Arc<Vec<u8>>>,
    pub converted: usize,
    pub refused: Vec<String>,
}

/// Convert and join inputs incrementally. Native import workers use this so they retain only the
/// current input and the growing output rather than every selected file's bytes.
pub(crate) fn build_create_multiple_output<I>(session: &pdfcraft_engine::Session, files: I) -> Result<CreateMultipleOutput, CreateMultipleFailure>
where
    I: IntoIterator<Item = crate::files::FileResult>,
{
    let (mut refused, mut converted) = (Vec::new(), 0usize);
    let mut files = files.into_iter();
    let sources = std::iter::from_fn(|| {
        loop {
            match files.next()? {
                Ok(file) => match crate::create_ui::convert_prepared_input_to_pdf(session, &file) {
                    Ok(pdf) => {
                        converted += 1;
                        return Some(Ok((stem(&file.name).to_string(), pdf, None)));
                    }
                    Err(_) => refused.push(file.name),
                },
                Err((name, error)) => return Some(Err(Stop::Read { name, error })),
            }
        }
    });
    let bytes = session.combine_each(sources).map_err(|error| match error {
        Stop::Read { name, error } => CreateMultipleFailure::Read { name, error },
        Stop::Build(error) => CreateMultipleFailure::Build(error.to_string()),
    })?;
    Ok(CreateMultipleOutput { bytes, converted, refused })
}

impl From<EditError> for Stop {
    fn from(e: EditError) -> Self {
        Stop::Build(e)
    }
}

impl PdfCraftApp {
    /// Convert each selected non-PDF file into its own PDF. Existing outputs and duplicate input
    /// names receive numbered names, so a batch never silently replaces an earlier file.
    pub(crate) fn stage_create_separate<I>(&mut self, files: I, dir: Option<PathBuf>)
    where
        I: IntoIterator<Item = Result<(String, Vec<u8>), (String, String)>>,
    {
        self.stage_create_separate_prepared(
            files.into_iter().map(|file| file.map(|(name, bytes)| crate::files::PreparedFile::unconverted(name, bytes))),
            dir,
        );
    }

    pub(crate) fn stage_create_separate_prepared<I>(&mut self, files: I, dir: Option<PathBuf>)
    where
        I: IntoIterator<Item = crate::files::FileResult>,
    {
        let mut used = HashSet::new();
        let mut made = 0usize;
        let mut refused = Vec::new();
        for file in files {
            let file = match file {
                Ok(file) => file,
                Err((name, error)) => {
                    refused.push(format!("{name} ({error})"));
                    continue;
                }
            };
            let name = file.name.clone();
            let kind = pdfcraft_engine::source_kind(&name, &file.bytes);
            if kind == Some(SourceKind::Pdf) {
                refused.push(format!("{name} ({})", tl!("already a PDF")));
                continue;
            }
            let pdf = match crate::create_ui::convert_prepared_input_to_pdf(&self.session, &file) {
                Ok(pdf) => pdf,
                Err(error) => {
                    refused.push(format!("{name} ({error})"));
                    continue;
                }
            };
            let base = crate::files::file_stem_from_user(stem(&name));
            let base = if base.is_empty() { "document" } else { &base };
            let mut output = None;
            for number in 1..10_000 {
                let filename = if number == 1 { format!("{base}.pdf") } else { format!("{base} ({number}).pdf") };
                let candidate = dir.as_ref().map(|d| d.join(&filename));
                if !used.contains(&filename.to_lowercase()) && !candidate.as_ref().is_some_and(|path| path.exists()) {
                    used.insert(filename.to_lowercase());
                    output = Some((filename, candidate));
                    break;
                }
            }
            let Some((filename, target)) = output else {
                refused.push(format!("{name} ({})", tl!("no unused output name")));
                continue;
            };
            #[cfg(not(target_arch = "wasm32"))]
            let result = match target {
                Some(path) => crate::editing::write_atomically(&path.to_string_lossy(), &pdf).map_err(|e| e.to_string()),
                None => Err(tl!("no output folder was chosen").to_string()),
            };
            #[cfg(target_arch = "wasm32")]
            let result = {
                let _ = target;
                crate::editing::download(&filename, &pdf)
            };
            match result {
                Ok(()) => made += 1,
                Err(error) => refused.push(format!("{filename} ({error})")),
            }
        }
        if made > 0 {
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(dir) = &dir {
                self.notify_fmt("Wrote {n} files to {dir}", &[("n", &made.to_string()), ("dir", &dir.display().to_string())]);
            }
            #[cfg(target_arch = "wasm32")]
            self.notify(crate::i18n::trn(crate::i18n::current(), made as u64, "Downloaded {n} PDF", "Downloaded {n} PDFs"));
        }
        if !refused.is_empty() {
            self.notify(crate::i18n::fmt(tl!("Left out: {names}"), &[("names", &refused.join(", "))]));
        }
    }

    /// Convert the picked files, join them in the order picked, and open the result in the page
    /// grid. A file that can't be converted is left out (and named in a notice); a file that can't be
    /// read stops the run. Files are read as their turn comes, so one file's bytes are held at a time.
    pub(crate) fn stage_create_multiple<I>(&mut self, files: I)
    where
        I: IntoIterator<Item = Result<(String, Vec<u8>), (String, String)>>,
    {
        self.stage_create_multiple_prepared(
            files.into_iter().map(|file| file.map(|(name, bytes)| crate::files::PreparedFile::unconverted(name, bytes))),
        );
    }

    pub(crate) fn stage_create_multiple_prepared<I>(&mut self, files: I)
    where
        I: IntoIterator<Item = crate::files::FileResult>,
    {
        let output = build_create_multiple_output(&self.session, files);
        self.finish_create_multiple(output);
    }

    pub(crate) fn finish_create_multiple(&mut self, output: Result<CreateMultipleOutput, CreateMultipleFailure>) {
        let output = match output {
            Ok(output) => output,
            Err(CreateMultipleFailure::Read { name, error }) => {
                self.notify_fmt("Couldn't read {name}: {e}", &[("name", &name), ("e", &error)]);
                return;
            }
            Err(CreateMultipleFailure::Build(error)) => {
                self.notify_fmt("Couldn't create a PDF: {e}", &[("e", &error)]);
                return;
            }
        };
        let Some(bytes) = output.bytes else {
            self.notify_tr("None of those files can be made into a PDF; use PDFs, images or .txt files");
            return;
        };
        let n = output.converted.to_string();
        let message = if !output.refused.is_empty() {
            crate::i18n::fmt(tl!("Created a PDF from {n} files; left out: {names}"), &[("n", &n), ("names", &output.refused.join(", "))])
        } else {
            crate::i18n::fmt(tl!("Created a PDF from {n} files"), &[("n", &n)])
        };
        let before = self.views.len();
        self.open_created("Combined.pdf", bytes, &message);
        if self.views.len() > before
            && let Some(view) = self.active.and_then(|i| self.views.get_mut(i))
        {
            view.open_organize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_multiple_uses_prepared_office_pdf_bytes() {
        let mut app = PdfCraftApp::new();
        let pdf = app.session.create_blank(200.0, 300.0, 1).unwrap();
        app.stage_create_multiple_prepared(vec![Ok(crate::files::PreparedFile {
            name: "report.docx".into(),
            bytes: b"synthetic Office source".to_vec(),
            modified: None,
            converted: Some(Ok(pdf)),
        })]);
        assert_eq!(app.views.len(), 1);
        let doc = app.session.get(app.views[0].id).unwrap();
        assert_eq!(doc.name, "Combined.pdf");
        assert_eq!(doc.info.pages.len(), 1);
    }

    #[test]
    fn create_multiple_reports_when_prepared_office_conversion_fails() {
        let mut app = PdfCraftApp::new();
        app.stage_create_multiple_prepared(vec![Ok(crate::files::PreparedFile {
            name: "report.docx".into(),
            bytes: b"synthetic Office source".to_vec(),
            modified: None,
            converted: Some(Err("synthetic conversion failure".into())),
        })]);
        assert!(app.views.is_empty());
        assert!(app.toast.is_some());
    }

    #[test]
    fn create_multiple_output_combines_inputs_incrementally_in_order() {
        let mut session = pdfcraft_engine::Session::new();
        let first = session.create_blank(200.0, 300.0, 1).unwrap();
        let second = session.create_blank(200.0, 300.0, 2).unwrap();
        let output = build_create_multiple_output(
            &session,
            vec![
                Ok(crate::files::PreparedFile::unconverted("first.pdf".into(), first.as_ref().clone())),
                Ok(crate::files::PreparedFile::unconverted("second.pdf".into(), second.as_ref().clone())),
            ],
        )
        .unwrap();
        let bytes = output.bytes.unwrap();
        assert_eq!(output.converted, 2);
        let doc = session.open_new("Combined.pdf", bytes).unwrap();
        assert_eq!(session.get(doc).unwrap().info.pages.len(), 3);
    }
}
