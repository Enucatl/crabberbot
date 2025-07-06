use crate::downloader::MediaMetadata;
use thiserror::Error;

// --- CHANGED: More descriptive constants for different media types ---
const MAX_DURATION_SECONDS: f64 = 600.0;
const MAX_FILESIZE_BYTES: u64 = 500 * 1024 * 1024; // 500 MB
const MAX_VIDEO_PLAYLIST_ITEMS: usize = 5;
const MAX_IMAGE_PLAYLIST_ITEMS: usize = 10; // New, larger limit for images/galleries

/// Represents the specific reasons why media metadata might be invalid.
#[derive(Error, Debug, PartialEq)]
pub enum ValidationError {
    #[error("The media is too long: {found:.0} minutes is over the {limit:.0} minute limit.")]
    TooLong { found: f64, limit: f64 },

    #[error("The media file is too large: {found_mb:.0} MB is over the {limit_mb:.0} MB limit.")]
    TooLarge { found_mb: u64, limit_mb: u64 },

    #[error("The playlist is too long: {found} items is more than the maximum of {limit}.")]
    TooManyItems { found: usize, limit: usize },
}

/// Validates the metadata of a media item or playlist against predefined limits.
///
/// # Arguments
/// * `metadata` - A reference to the `MediaMetadata` fetched from yt-dlp.
///
/// # Returns
/// * `Ok(())` if the metadata is valid.
/// * `Err(ValidationError)` if the metadata exceeds any of the limits.
pub fn validate_media_metadata(metadata: &MediaMetadata) -> Result<(), ValidationError> {
    if let Some(entries) = &metadata.entries {
        // We check the first item in the playlist to determine the content type.
        let is_video_playlist = entries
            .first()
            .and_then(|entry| entry.media_type.as_ref())
            .map_or(false, |m_type| m_type == "video");

        let limit = if is_video_playlist {
            MAX_VIDEO_PLAYLIST_ITEMS
        } else {
            // Default to the larger limit for image galleries or mixed types.
            MAX_IMAGE_PLAYLIST_ITEMS
        };

        if entries.len() > limit {
            return Err(ValidationError::TooManyItems {
                found: entries.len(),
                limit,
            });
        }
    } else {
        // This is a single item, not a playlist. Check its properties.
        if let Some(duration) = metadata.duration {
            if duration > MAX_DURATION_SECONDS {
                return Err(ValidationError::TooLong {
                    found: duration / 60.0,
                    limit: MAX_DURATION_SECONDS / 60.0,
                });
            }
        }
        if let Some(filesize) = metadata.filesize {
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
    use crate::test_utils::create_test_metadata;

    #[test]
    fn test_valid_single_item() {
        let mut metadata = create_test_metadata();
        metadata.duration = Some(MAX_DURATION_SECONDS / 2.0);
        metadata.filesize = Some(MAX_FILESIZE_BYTES - 1);
        assert!(validate_media_metadata(&metadata).is_ok());
    }

    #[test]
    fn test_item_too_long() {
        let mut metadata = create_test_metadata();
        let duration = MAX_DURATION_SECONDS + 1.0;
        metadata.duration = Some(duration);
        assert_eq!(
            validate_media_metadata(&metadata).unwrap_err(),
            ValidationError::TooLong {
                found: duration / 60.0,
                limit: MAX_DURATION_SECONDS / 60.0
            }
        );
    }

    #[test]
    fn test_item_too_large() {
        let mut metadata = create_test_metadata();
        let size = MAX_FILESIZE_BYTES + 1;
        metadata.filesize = Some(size);
        assert_eq!(
            validate_media_metadata(&metadata).unwrap_err(),
            ValidationError::TooLarge {
                found_mb: size / 1024 / 1024,
                limit_mb: MAX_FILESIZE_BYTES / 1024 / 1024,
            }
        );
    }

    // --- NEW AND UPDATED TESTS FOR PLAYLISTS ---

    #[test]
    fn test_valid_video_playlist() {
        let mut metadata = create_test_metadata();
        let mut video_entry = create_test_metadata();
        video_entry.media_type = Some("video".to_string());
        metadata.entries = Some(vec![video_entry; MAX_VIDEO_PLAYLIST_ITEMS]);
        assert!(validate_media_metadata(&metadata).is_ok());
    }

    #[test]
    fn test_video_playlist_too_many_items() {
        let mut metadata = create_test_metadata();
        let n_items = MAX_VIDEO_PLAYLIST_ITEMS + 1;
        let mut video_entry = create_test_metadata();
        video_entry.media_type = Some("video".to_string());
        metadata.entries = Some(vec![video_entry; n_items]);
        assert_eq!(
            validate_media_metadata(&metadata).unwrap_err(),
            ValidationError::TooManyItems {
                found: n_items,
                limit: MAX_VIDEO_PLAYLIST_ITEMS,
            }
        );
    }

    #[test]
    fn test_valid_image_playlist() {
        let mut metadata = create_test_metadata();
        // Use a number of items over the video limit but under the image limit
        let n_items = MAX_IMAGE_PLAYLIST_ITEMS - 1;
        assert!(n_items > MAX_VIDEO_PLAYLIST_ITEMS);

        let mut image_entry = create_test_metadata();
        // The type is not "video", so it should use the larger limit.
        image_entry.media_type = Some("image".to_string());
        metadata.entries = Some(vec![image_entry; n_items]);

        assert!(validate_media_metadata(&metadata).is_ok());
    }

    #[test]
    fn test_image_playlist_too_many_items() {
        let mut metadata = create_test_metadata();
        let n_items = MAX_IMAGE_PLAYLIST_ITEMS + 1;
        let mut image_entry = create_test_metadata();
        image_entry.media_type = Some("image".to_string()); // A non-video type
        metadata.entries = Some(vec![image_entry; n_items]);
        assert_eq!(
            validate_media_metadata(&metadata).unwrap_err(),
            ValidationError::TooManyItems {
                found: n_items,
                limit: MAX_IMAGE_PLAYLIST_ITEMS,
            }
        );
    }

    #[test]
    fn test_playlist_with_no_type_uses_image_limit() {
        let mut metadata = create_test_metadata();
        // This count is too high for videos, but okay for images.
        let n_items = MAX_VIDEO_PLAYLIST_ITEMS + 1;
        let mut untyped_entry = create_test_metadata();
        // media_type is None
        untyped_entry.media_type = None;
        metadata.entries = Some(vec![untyped_entry; n_items]);

        // It should be OK because the default is the lenient image limit.
        assert!(validate_media_metadata(&metadata).is_ok());
    }

    #[test]
    fn test_single_item_with_no_metadata_is_valid() {
        let metadata = create_test_metadata();
        assert!(validate_media_metadata(&metadata).is_ok());
    }
}
