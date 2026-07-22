use crate::downloader::MediaInfo;
use thiserror::Error;

const MAX_DURATION_SECONDS: f64 = 1800.0;
pub(crate) const MAX_FILESIZE_BYTES: u64 = 500 * 1024 * 1024; // 500 MiB
const MAX_PLAYLIST_ITEMS: usize = 20;

#[derive(Error, Debug, PartialEq)]
pub enum ValidationError {
    #[error("The media metadata is incomplete or invalid. Please try a different link.")]
    InvalidMetadata,

    #[error("The media is too long: {found:.0} minutes is over the {limit:.0} minute limit.")]
    TooLong { found: f64, limit: f64 },

    #[error("The media file is too large: {found_mb:.0} MB is over the {limit_mb:.0} MB limit.")]
    TooLarge { found_mb: u64, limit_mb: u64 },

    #[error("The playlist is too long: {found} items is more than the maximum of {limit}.")]
    TooManyItems { found: usize, limit: usize },
}

pub fn validate_media_metadata(info: &MediaInfo) -> Result<(), ValidationError> {
    if let Some(entries) = &info.entries {
        if entries.is_empty() {
            return Err(ValidationError::InvalidMetadata);
        }

        if entries.len() > MAX_PLAYLIST_ITEMS {
            return Err(ValidationError::TooManyItems {
                found: entries.len(),
                limit: MAX_PLAYLIST_ITEMS,
            });
        }

        validate_optional_metadata(info)?;
        for entry in entries {
            validate_single_item(entry)?;
        }
    } else {
        validate_single_item(info)?;
    }
    Ok(())
}

fn validate_single_item(info: &MediaInfo) -> Result<(), ValidationError> {
    match info.filesize {
        Some(0) => return Err(ValidationError::InvalidMetadata),
        Some(filesize) => validate_filesize(filesize)?,
        None => {}
    }

    validate_optional_duration(info.duration)
}

fn validate_optional_metadata(info: &MediaInfo) -> Result<(), ValidationError> {
    if let Some(filesize) = info.filesize {
        if filesize == 0 {
            return Err(ValidationError::InvalidMetadata);
        }
        validate_filesize(filesize)?;
    }
    validate_optional_duration(info.duration)
}

fn validate_filesize(filesize: u64) -> Result<(), ValidationError> {
    if filesize > MAX_FILESIZE_BYTES {
        return Err(ValidationError::TooLarge {
            found_mb: filesize / 1024 / 1024,
            limit_mb: MAX_FILESIZE_BYTES / 1024 / 1024,
        });
    }
    Ok(())
}

fn validate_optional_duration(duration: Option<f64>) -> Result<(), ValidationError> {
    if let Some(duration) = duration {
        if !duration.is_finite() || duration < 0.0 {
            return Err(ValidationError::InvalidMetadata);
        }
        if duration > MAX_DURATION_SECONDS {
            return Err(ValidationError::TooLong {
                found: duration / 60.0,
                limit: MAX_DURATION_SECONDS / 60.0,
            });
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
        info.media_type = Some("video".to_string());
        info.duration = Some(MAX_DURATION_SECONDS / 2.0);
        info.filesize = Some(MAX_FILESIZE_BYTES - 1);
        assert!(validate_media_metadata(&info).is_ok());
    }

    #[test]
    fn test_item_too_long() {
        let mut info = create_test_info();
        info.media_type = Some("video".to_string());
        let duration = MAX_DURATION_SECONDS + 1.0;
        info.filesize = Some(1);
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
        info.media_type = Some("video".to_string());
        info.duration = Some(0.0);
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
        video_entry.duration = Some(0.0);
        video_entry.filesize = Some(1);
        info.entries = Some(vec![video_entry; MAX_PLAYLIST_ITEMS]);
        assert!(validate_media_metadata(&info).is_ok());
    }

    #[test]
    fn test_playlist_too_many_items() {
        let mut info = create_test_info();
        let n_items = MAX_PLAYLIST_ITEMS + 1;
        let mut video_entry = create_test_info();
        video_entry.media_type = Some("video".to_string());
        video_entry.duration = Some(0.0);
        video_entry.filesize = Some(1);
        info.entries = Some(vec![video_entry; n_items]);
        assert_eq!(
            validate_media_metadata(&info).unwrap_err(),
            ValidationError::TooManyItems {
                found: n_items,
                limit: MAX_PLAYLIST_ITEMS,
            }
        );
    }

    #[test]
    fn test_valid_image_playlist() {
        let mut info = create_test_info();
        let n_items = MAX_PLAYLIST_ITEMS;

        let mut image_entry = create_test_info();
        image_entry.media_type = Some("image".to_string());
        image_entry.filesize = Some(1);
        info.entries = Some(vec![image_entry; n_items]);

        assert!(validate_media_metadata(&info).is_ok());
    }

    #[test]
    fn test_image_playlist_too_many_items() {
        let mut info = create_test_info();
        let n_items = MAX_PLAYLIST_ITEMS + 1;
        let mut image_entry = create_test_info();
        image_entry.media_type = Some("image".to_string());
        image_entry.filesize = Some(1);
        info.entries = Some(vec![image_entry; n_items]);
        assert_eq!(
            validate_media_metadata(&info).unwrap_err(),
            ValidationError::TooManyItems {
                found: n_items,
                limit: MAX_PLAYLIST_ITEMS,
            }
        );
    }

    #[test]
    fn test_unknown_playlist_type_uses_shared_limit() {
        let mut info = create_test_info();
        let n_items = MAX_PLAYLIST_ITEMS + 1;
        let mut untyped_entry = create_test_info();
        untyped_entry.media_type = None;
        untyped_entry.duration = Some(0.0);
        untyped_entry.filesize = Some(1);
        info.entries = Some(vec![untyped_entry; n_items]);

        assert_eq!(
            validate_media_metadata(&info).unwrap_err(),
            ValidationError::TooManyItems {
                found: n_items,
                limit: MAX_PLAYLIST_ITEMS,
            }
        );
    }

    #[test]
    fn test_mixed_playlist_uses_shared_limit() {
        let mut info = create_test_info();
        let mut image_entry = create_test_info();
        image_entry.media_type = Some("image".to_string());
        image_entry.duration = None;
        let entries = (0..MAX_PLAYLIST_ITEMS + 1)
            .map(|index| {
                if index == 0 {
                    create_test_info()
                } else {
                    image_entry.clone()
                }
            })
            .collect();
        info.entries = Some(entries);

        assert_eq!(
            validate_media_metadata(&info).unwrap_err(),
            ValidationError::TooManyItems {
                found: MAX_PLAYLIST_ITEMS + 1,
                limit: MAX_PLAYLIST_ITEMS,
            }
        );
    }

    #[test]
    fn test_invalid_item_metadata_is_rejected_but_missing_fields_are_allowed() {
        let mut info = create_test_info();
        info.media_type = Some("video".to_string());
        info.filesize = Some(0);
        info.duration = Some(0.0);
        assert_eq!(
            validate_media_metadata(&info),
            Err(ValidationError::InvalidMetadata)
        );
        info.filesize = None;
        info.duration = None;
        assert!(validate_media_metadata(&info).is_ok());
        info.filesize = Some(1);
        for duration in [Some(-1.0), Some(f64::NAN), Some(f64::INFINITY)] {
            info.duration = duration;
            assert_eq!(
                validate_media_metadata(&info),
                Err(ValidationError::InvalidMetadata)
            );
        }
    }

    #[test]
    fn test_durationless_image_and_boundaries_are_valid() {
        let mut info = create_test_info();
        info.media_type = Some("image".to_string());
        info.filesize = Some(MAX_FILESIZE_BYTES);
        assert!(validate_media_metadata(&info).is_ok());

        info.duration = Some(MAX_DURATION_SECONDS);
        assert!(validate_media_metadata(&info).is_ok());
    }

    #[test]
    fn test_empty_or_invalid_playlist_entry_is_rejected() {
        let mut info = create_test_info();
        info.entries = Some(vec![]);
        assert_eq!(
            validate_media_metadata(&info),
            Err(ValidationError::InvalidMetadata)
        );

        let mut valid_entry = create_test_info();
        valid_entry.media_type = Some("video".to_string());
        valid_entry.duration = Some(0.0);
        valid_entry.filesize = Some(1);
        let mut invalid_entry = valid_entry.clone();
        invalid_entry.filesize = Some(MAX_FILESIZE_BYTES + 1);
        info.entries = Some(vec![valid_entry, invalid_entry]);
        assert!(matches!(
            validate_media_metadata(&info),
            Err(ValidationError::TooLarge { .. })
        ));
    }

    #[test]
    fn test_playlist_container_metadata_is_optional_but_validated_when_supplied() {
        let mut image = create_test_info();
        image.media_type = Some("image".to_string());
        image.duration = None;
        let mut info = create_test_info();
        info.duration = None;
        info.filesize = None;
        info.entries = Some(vec![image]);
        assert!(validate_media_metadata(&info).is_ok());

        info.filesize = Some(0);
        assert_eq!(
            validate_media_metadata(&info),
            Err(ValidationError::InvalidMetadata)
        );
        info.filesize = None;
        info.duration = Some(f64::NAN);
        assert_eq!(
            validate_media_metadata(&info),
            Err(ValidationError::InvalidMetadata)
        );
    }
}
