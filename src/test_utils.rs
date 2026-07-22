use crate::downloader::MediaInfo;

pub fn create_test_info() -> MediaInfo {
    MediaInfo {
        id: "123".to_string(),
        media_type: Some("video".to_string()),
        thumbnail: Some("http://example.com/thumb.jpg".to_string()),
        duration: Some(1.0),
        filesize: Some(1),
        ..Default::default()
    }
}
