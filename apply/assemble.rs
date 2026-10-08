//! Assemble generated entries without depending on cloud contracts or tools.

use std::{fs, io, path::Path};

pub fn assemble(root: &Path) -> io::Result<String> {
    let mut sections = Vec::new();
    for category in ["resources", "messages", "enums", "services"] {
        // Git does not preserve an empty directory after its last entry is removed.
        let mut paths = match fs::read_dir(root.join(category)) {
            Ok(entries) => entries
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<io::Result<Vec<_>>>()?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        paths.retain(|path| path.extension().is_some_and(|ext| ext == "json"));
        paths.sort();
        let entries = paths
            .into_iter()
            .map(fs::read_to_string)
            .collect::<io::Result<Vec<_>>>()?;
        sections.push(format!("\"{category}\":[{}]", entries.join(",")));
    }
    Ok(format!("{{{}}}\n", sections.join(",")))
}
