use crate::feed::Episode;
use crate::sanitize::sanitize_stem;
use anyhow::Context;
use std::path::{Path, PathBuf};

/// Name of the URL-list file for a feed: sanitized channel title + ".txt",
/// falling back to "torrents.txt" when the title sanitizes to nothing.
/// Also used by the step-3 prompt label so the UI names the real file.
pub fn url_list_filename(channel_title: &str) -> String {
    let stem = sanitize_stem(channel_title);
    let stem = if stem.is_empty() { "torrents".to_string() } else { stem };
    format!("{stem}.txt")
}

/// Write one torrent URL per line to `url_list_filename(channel_title)` in
/// `dir`, overwriting any previous list. Returns the file's path.
pub fn write_url_list(
    episodes: &[Episode],
    dir: &Path,
    channel_title: &str,
) -> anyhow::Result<PathBuf> {
    let target = dir.join(url_list_filename(channel_title));

    let mut body = String::new();
    for ep in episodes {
        body.push_str(&ep.torrent_url);
        body.push('\n');
    }
    std::fs::write(&target, body).with_context(|| format!("writing {}", target.display()))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::Episode;

    fn ep(title: &str, url: &str) -> Episode {
        Episode {
            title: title.to_string(),
            torrent_url: url.to_string(),
            size: None,
            pub_date: None,
        }
    }

    #[test]
    fn writes_one_url_per_line_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let eps = vec![ep("A", "http://x/1.torrent"), ep("B", "http://x/2.torrent")];
        let path = write_url_list(&eps, dir.path(), "石纪元 科学与未来").unwrap();
        assert_eq!(path.file_name().unwrap().to_string_lossy(), "石纪元 科学与未来.txt");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "http://x/1.torrent\nhttp://x/2.torrent\n"
        );
    }

    #[test]
    fn sanitizes_channel_title_in_filename() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_url_list(&[ep("A", "http://x/1.torrent")], dir.path(), "A / B: C").unwrap();
        assert_eq!(path.file_name().unwrap().to_string_lossy(), "A ⁄ B_ C.txt");
    }

    #[test]
    fn overwrites_existing_list() {
        let dir = tempfile::tempdir().unwrap();
        write_url_list(&[ep("A", "http://x/old.torrent")], dir.path(), "t").unwrap();
        let path = write_url_list(&[ep("B", "http://x/new.torrent")], dir.path(), "t").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "http://x/new.torrent\n");
    }

    #[test]
    fn empty_title_falls_back_to_torrents() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_url_list(&[ep("A", "http://x/1.torrent")], dir.path(), "...").unwrap();
        assert_eq!(path.file_name().unwrap().to_string_lossy(), "torrents.txt");
    }
}
