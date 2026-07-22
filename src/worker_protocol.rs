use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::downloader::{DownloadedItem, DownloadedMedia, MediaInfo, MediaType};

pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "snake_case")]
pub enum Request {
    Metadata {
        url: String,
    },
    Download {
        info: MediaInfo,
        url: String,
    },
    ExtractAudio {
        video_path: String,
        title: Option<String>,
        author: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok { result: ResultData },
    Error { kind: String, message: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResultData {
    Metadata {
        info: MediaInfo,
    },
    Download {
        media: WireMedia,
    },
    Audio {
        audio_path: String,
        duration_secs: i32,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireMedia {
    Single { item: WireItem },
    Group { items: Vec<WireItem> },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WireItem {
    pub filepath: String,
    pub media_type: MediaType,
    pub thumbnail_filepath: Option<String>,
}

impl From<DownloadedMedia> for WireMedia {
    fn from(media: DownloadedMedia) -> Self {
        match media {
            DownloadedMedia::Single(item) => Self::Single { item: item.into() },
            DownloadedMedia::Group(items) => Self::Group {
                items: items.into_iter().map(Into::into).collect(),
            },
        }
    }
}

impl From<DownloadedItem> for WireItem {
    fn from(item: DownloadedItem) -> Self {
        Self {
            filepath: item.filepath.to_string_lossy().into_owned(),
            media_type: item.media_type,
            thumbnail_filepath: item
                .thumbnail_filepath
                .map(|path| path.to_string_lossy().into_owned()),
        }
    }
}

impl TryFrom<WireMedia> for DownloadedMedia {
    type Error = &'static str;

    fn try_from(media: WireMedia) -> Result<Self, Self::Error> {
        let item = |item: WireItem| DownloadedItem {
            filepath: PathBuf::from(item.filepath),
            media_type: item.media_type,
            thumbnail_filepath: item.thumbnail_filepath.map(PathBuf::from),
        };
        Ok(match media {
            WireMedia::Single { item: wire } => Self::Single(item(wire)),
            WireMedia::Group { items } => Self::Group(items.into_iter().map(item).collect()),
        })
    }
}

pub async fn read_frame<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Vec<u8>, String> {
    let length = stream.read_u32().await.map_err(|error| error.to_string())? as usize;
    if length > MAX_FRAME_BYTES {
        return Err("frame exceeds 1 MiB".to_string());
    }
    let mut frame = vec![0; length];
    stream
        .read_exact(&mut frame)
        .await
        .map_err(|error| error.to_string())?;
    Ok(frame)
}

pub async fn write_frame<S: AsyncWrite + Unpin>(
    stream: &mut S,
    value: &impl Serialize,
) -> Result<(), String> {
    let frame = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    if frame.len() > MAX_FRAME_BYTES {
        return Err("frame exceeds 1 MiB".to_string());
    }
    stream
        .write_u32(frame.len() as u32)
        .await
        .map_err(|error| error.to_string())?;
    stream
        .write_all(&frame)
        .await
        .map_err(|error| error.to_string())?;
    stream.flush().await.map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_and_reject_oversize() {
        let (mut left, mut right) = tokio::io::duplex(MAX_FRAME_BYTES + 8);
        let request = Request::Metadata {
            url: "https://example.com".to_string(),
        };
        let writer = tokio::spawn(async move { write_frame(&mut left, &request).await });
        let frame = read_frame(&mut right).await.unwrap();
        writer.await.unwrap().unwrap();
        assert!(matches!(
            serde_json::from_slice::<Request>(&frame).unwrap(),
            Request::Metadata { .. }
        ));
        let (mut oversized_writer, mut oversized_reader) = tokio::io::duplex(8);
        oversized_writer
            .write_u32((MAX_FRAME_BYTES + 1) as u32)
            .await
            .unwrap();
        assert!(read_frame(&mut oversized_reader).await.is_err());
    }
}
