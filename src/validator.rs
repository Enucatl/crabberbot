use thiserror::Error;
use crate::downloader::MediaMetadata;

// These are our limits. Placing them here makes them easy to find and change.
const MAX_DURATION_SECONDS: f64 = 600.0;
const MAX_FILESIZE_BYTES: u64 = 500 * 1024 * 1024; // 500 MB
const MAX_PLAYLIST_ITEMS: usize = 5;

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
    // First, check if it's a playlist by looking at the 'entries' field.
    if let Some(entries) = &metadata.entries {
        if entries.len() > MAX_PLAYLIST_ITEMS {
            return Err(ValidationError::TooManyItems {
                found: entries.len(),
                limit: MAX_PLAYLIST_ITEMS,
            });
        }
    } else {
        // If it's not a playlist, it's a single item. Check its properties.
        if let Some(duration) = metadata.duration {
            if duration > MAX_DURATION_SECONDS {
                return Err(ValidationError::TooLong {
                    found: duration / 60.0, // Convert to minutes for the error message
                    limit: MAX_DURATION_SECONDS / 60.0,
                });
            }
        }
        if let Some(filesize) = metadata.filesize {
            if filesize > MAX_FILESIZE_BYTES {
                return Err(ValidationError::TooLarge {
                    found_mb: filesize / 1024 / 1024, // Convert to MB for the error message
                    limit_mb: MAX_FILESIZE_BYTES / 1024 / 1024,
                });
            }
        }
    }
    // If we've reached this point, all checks have passed.
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*; // Imports everything from the parent module (validator)
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
        let duration = MAX_DURATION_SECONDS * 2.0;
        metadata.duration = Some(duration); 

        let result = validate_media_metadata(&metadata);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            ValidationError::TooLong {
                found: duration / 60.0,
                limit: MAX_DURATION_SECONDS / 60.0
            }
        );
    }
    
    #[test]
    fn test_item_too_large() {
        let mut metadata = create_test_metadata();
        let size = MAX_FILESIZE_BYTES * 2;
        metadata.filesize = Some(size);

        let result = validate_media_metadata(&metadata);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            ValidationError::TooLarge {
                found_mb: size / 1024 / 1024,
                limit_mb: MAX_FILESIZE_BYTES / 1024 / 1024,
            }
        );
    }
    
    #[test]
    fn test_valid_playlist() {
        let mut metadata = create_test_metadata();
        metadata.entries = Some(vec![create_test_metadata(); MAX_PLAYLIST_ITEMS - 1]);
        assert!(validate_media_metadata(&metadata).is_ok());
    }

    #[test]
    fn test_playlist_too_many_items() {
        let mut metadata = create_test_metadata();
        let n_items = MAX_PLAYLIST_ITEMS * 2;
        metadata.entries = Some(vec![create_test_metadata(); n_items]);

        let result = validate_media_metadata(&metadata);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            ValidationError::TooManyItems {
                found: n_items,
                limit: MAX_PLAYLIST_ITEMS,
            }
        );
    }

    #[test]
    fn test_single_item_with_no_metadata_is_valid() {
        // A common case is yt-dlp not providing duration or filesize.
        // The bot should proceed in this case, not fail.
        let metadata = create_test_metadata();
        assert!(validate_media_metadata(&metadata).is_ok());
    }
}
