use crate::downloader::MediaInfo;

pub fn create_test_info() -> MediaInfo {
    MediaInfo {
        id: "123".to_string(),
        description: Some("".to_string()),
        duration: None,
        entries: None,
        filesize: None,
        height: None,
        media_type: None,
        playlist_uploader: None,
        resolution: None,
        thumbnail: Some("http://example.com/thumb.jpg".to_string()),
        title: Some("".to_string()),
        uploader: None,
        width: None,
    }
}
