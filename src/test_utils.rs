// This file will contain shared helper functions for our tests.
use crate::downloader::MediaMetadata;

// This is the single, authoritative helper function.
pub fn create_test_metadata() -> MediaMetadata {
    MediaMetadata {
        id: "123".to_string(),
        description: Some("".to_string()),
        duration: None,
        entries: None,
        ext: Some("".to_string()),
        filepath: Some("".to_string()),
        filesize: None,
        final_caption: "".to_string(),
        height: None,
        media_type: None,
        playlist_uploader: None,
        resolution: None,
        title: Some("".to_string()),
        uploader: None,
        width: None,
    }
}
