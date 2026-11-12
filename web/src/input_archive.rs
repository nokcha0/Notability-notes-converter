use std::io::{Cursor, Read};
use zip::ZipArchive;

const MAX_NOTE_BYTES: u64 = 512 * 1024 * 1024;

pub(crate) struct InputZip {
    bundle: ZipArchive<Cursor<Vec<u8>>>,
    entries: Vec<(usize, String)>,
}

// Archive paths are kept for the output ZIP, never extracted to a filesystem.
fn note_path(name: &str) -> Option<String> {
    if name.starts_with(['/', '\\']) || name.contains(':') || name.contains('\0') {
        return None;
    }
    let mut parts = Vec::new();
    for part in name.split(['/', '\\']) {
        match part {
            ".." => return None,
            "" | "." => continue,
            "__MACOSX" => return None,
            _ => parts.push(part),
        }
    }
    let file = parts.last()?;
    if file.starts_with("._")
        || !file.rsplit_once('.').is_some_and(|(_, extension)| {
            extension.eq_ignore_ascii_case("note") || extension.eq_ignore_ascii_case("ntb")
        })
    {
        return None;
    }
    Some(parts.join("/"))
}

impl InputZip {
    pub(crate) fn new(bytes: Vec<u8>) -> Result<Self, String> {
        let mut bundle =
            ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("Could not open ZIP: {e}"))?;
        let mut entries = Vec::new();
        for index in 0..bundle.len() {
            let entry = bundle.by_index_raw(index).map_err(|e| e.to_string())?;
            if !entry.is_dir() {
                if let Some(path) = note_path(entry.name()) {
                    entries.push((index, path));
                }
            }
        }
        Ok(Self { bundle, entries })
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.entries.iter().map(|(_, name)| name.clone()).collect()
    }

    pub(crate) fn read(&mut self, index: usize) -> Result<(Vec<u8>, String), String> {
        let (zip_index, name) = self.entries.get(index).ok_or("Invalid ZIP entry")?;
        let file = self
            .bundle
            .by_index(*zip_index)
            .map_err(|e| e.to_string())?;
        if file.size() > MAX_NOTE_BYTES {
            return Err("Note exceeds the 512 MB ZIP import limit".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_NOTE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_NOTE_BYTES {
            return Err("Note exceeds the 512 MB ZIP import limit".into());
        }
        Ok((bytes, name.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::{write::SimpleFileOptions, ZipWriter};

    #[test]
    fn reads_only_notes_and_keeps_safe_folder_names() {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        for name in [
            "Course/First.note",
            "Course/Second.NTB",
            "readme.txt",
            "../bad.note",
            "__MACOSX/._First.note",
            "/bad.ntb",
            "C:\\bad.note",
            "Course/._Second.ntb",
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(name.as_bytes()).unwrap();
        }
        let mut input = InputZip::new(zip.finish().unwrap().into_inner()).unwrap();
        assert_eq!(input.names(), ["Course/First.note", "Course/Second.NTB"]);
        assert_eq!(
            input.read(1).unwrap(),
            (b"Course/Second.NTB".to_vec(), "Course/Second.NTB".into())
        );
        assert!(input.read(2).is_err());
        assert_eq!(
            note_path("Course\\First.note"),
            Some("Course/First.note".into())
        );
    }

    #[test]
    fn reports_corrupt_archives_and_accepts_empty_ones() {
        assert!(InputZip::new(b"not zip".to_vec()).is_err());
        let zip = ZipWriter::new(Cursor::new(Vec::new()))
            .finish()
            .unwrap()
            .into_inner();
        assert!(InputZip::new(zip).unwrap().names().is_empty());
    }
}
