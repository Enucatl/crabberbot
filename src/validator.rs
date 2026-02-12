use crate::downloader::MediaInfo;
use thiserror::Error;

const MAX_DURATION_SECONDS: f64 = 1800.0;
const MAX_FILESIZE_BYTES: u64 = 500 * 1024 * 1024; // 500 MB
const MAX_VIDEO_PLAYLIST_ITEMS: usize = 5;
const MAX_IMAGE_PLAYLIST_ITEMS: usize = 10;

#[derive(Error, Debug, PartialEq)]
pub enum ValidationError {
    #[error("The media is too long: {found:.0} minutes is over the {limit:.0} minute limit.")]
    TooLong { found: f64, limit: f64 },

    #[error("The media file is too large: {found_mb:.0} MB is over the {limit_mb:.0} MB limit.")]
    TooLarge { found_mb: u64, limit_mb: u64 },

    #[error("The playlist is too long: {found} items is more than the maximum of {limit}.")]
    TooManyItems { found: usize, limit: usize },
}

pub fn validate_media_metadata(info: &MediaInfo) -> Result<(), ValidationError> {
    if let Some(entries) = &info.entries {
        let is_video_playlist = entries
            .first()
            .and_then(|entry| entry.media_type.as_ref())
            .is_some_and(|m_type| m_type == "video");

        let limit = if is_video_playlist {
            MAX_VIDEO_PLAYLIST_ITEMS
        } else {
            MAX_IMAGE_PLAYLIST_ITEMS
        };

        if entries.len() > limit {
            return Err(ValidationError::TooManyItems {
                found: entries.len(),
                limit,
            });
        }
    } else {
        if let Some(duration) = info.duration {
            if duration > MAX_DURATION_SECONDS {
                return Err(ValidationError::TooLong {
                    found: duration / 60.0,
                    limit: MAX_DURATION_SECONDS / 60.0,
                });
            }
        }
        if let Some(filesize) = info.filesize {
            if filesize > MAX_FILESIZE_BYTES {
                return Err(ValidationError::TooLarge {
                    found_mb: filesize / 1024 / 1024,
                    limit_mb: MAX_FILESIZE_BYTES / 1024 / 1024,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::create_test_info;

    #[test]
    fn test_valid_single_item() {
        let mut info = create_test_info();
        info.duration = Some(MAX_DURATION_SECONDS / 2.0);
        info.filesize = Some(MAX_FILESIZE_BYTES - 1);
        assert!(validate_media_metadata(&info).is_ok());
    }

    #[test]
    fn test_item_too_long() {
        let mut info = create_test_info();
        let duration = MAX_DURATION_SECONDS + 1.0;
        info.duration = Some(duration);
        assert_eq!(
            validate_media_metadata(&info).unwrap_err(),
            ValidationError::TooLong {
                found: duration / 60.0,
                limit: MAX_DURATION_SECONDS / 60.0
            }
        );
    }

    #[test]
    fn test_item_too_large() {
        let mut info = create_test_info();
        let size = MAX_FILESIZE_BYTES + 1;
        info.filesize = Some(size);
        assert_eq!(
            validate_media_metadata(&info).unwrap_err(),
            ValidationError::TooLarge {
                found_mb: size / 1024 / 1024,
                limit_mb: MAX_FILESIZE_BYTES / 1024 / 1024,
            }
        );
    }

    #[test]
    fn test_valid_video_playlist() {
        let mut info = create_test_info();
        let mut video_entry = create_test_info();
        video_entry.media_type = Some("video".to_string());
        info.entries = Some(vec![video_entry; MAX_VIDEO_PLAYLIST_ITEMS]);
        assert!(validate_media_metadata(&info).is_ok());
    }

    #[test]
    fn test_video_playlist_too_many_items() {
        let mut info = create_test_info();
        let n_items = MAX_VIDEO_PLAYLIST_ITEMS + 1;
        let mut video_entry = create_test_info();
        video_entry.media_type = Some("video".to_string());
        info.entries = Some(vec![video_entry; n_items]);
        assert_eq!(
            validate_media_metadata(&info).unwrap_err(),
            ValidationError::TooManyItems {
                found: n_items,
                limit: MAX_VIDEO_PLAYLIST_ITEMS,
            }
        );
    }

    #[test]
    fn test_valid_image_playlist() {
        let mut info = create_test_info();
        let n_items = MAX_IMAGE_PLAYLIST_ITEMS - 1;
        assert!(n_items > MAX_VIDEO_PLAYLIST_ITEMS);

        let mut image_entry = create_test_info();
        image_entry.media_type = Some("image".to_string());
        info.entries = Some(vec![image_entry; n_items]);

        assert!(validate_media_metadata(&info).is_ok());
    }

    #[test]
    fn test_image_playlist_too_many_items() {
        let mut info = create_test_info();
        let n_items = MAX_IMAGE_PLAYLIST_ITEMS + 1;
        let mut image_entry = create_test_info();
        image_entry.media_type = Some("image".to_string());
        info.entries = Some(vec![image_entry; n_items]);
        assert_eq!(
            validate_media_metadata(&info).unwrap_err(),
            ValidationError::TooManyItems {
                found: n_items,
                limit: MAX_IMAGE_PLAYLIST_ITEMS,
            }
        );
    }

    #[test]
    fn test_playlist_with_no_type_uses_image_limit() {
        let mut info = create_test_info();
        let n_items = MAX_VIDEO_PLAYLIST_ITEMS + 1;
        let mut untyped_entry = create_test_info();
        untyped_entry.media_type = None;
        info.entries = Some(vec![untyped_entry; n_items]);

        assert!(validate_media_metadata(&info).is_ok());
    }

    #[test]
    fn test_single_item_with_no_metadata_is_valid() {
        let info = create_test_info();
        assert!(validate_media_metadata(&info).is_ok());
    }
}
