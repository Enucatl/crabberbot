use crate::downloader::MediaInfo;

pub fn create_test_info() -> MediaInfo {
    MediaInfo {
        id: "123".to_string(),
        thumbnail: Some("http://example.com/thumb.jpg".to_string()),
        ..Default::default()
    }
}
