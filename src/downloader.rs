#![allow(dead_code)]
use crate::chunk::{Chunk, ChunkVec, Chunks};
use crate::multi::Downloaded;
use crate::Hash;
use crate::ManicError;
use crate::Result;
use indicatif::ProgressBar;
use reqwest::header::{CONTENT_LENGTH, RANGE};
use reqwest::Client;
use std::path::Path;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;
use tracing::{debug, instrument};

#[derive(Debug, Clone)]
pub struct Downloader {
    filename: String,
    client: Client,
    workers: u8,
    url: reqwest::Url,
    hash: Option<Hash>,
    length: u64,
    chunks: Chunks,
    #[cfg(feature = "progress")]
    pb: Option<indicatif::ProgressBar>,
}

impl Downloader {
    pub fn get_client(&self) -> &Client {
        &self.client
    }
    pub fn get_url(&self) -> String {
        self.url.to_string()
    }
    pub fn get_len(&self) -> u64 {
        self.length
    }
    pub fn filename(&self) -> &str {
        &self.filename
    }
    async fn assemble_downloader(
        url: &str,
        workers: u8,
        length: u64,
        client: Client,
    ) -> Result<Self> {
        let parsed = reqwest::Url::parse(url)?;
        if length == 0 {
            return Err(ManicError::NoLen);
        }
        let chunks = Chunks::new(0, length - 1, length / workers as u64)?;
        let filename = Self::url_to_filename(&parsed)?;
        #[cfg(not(feature = "progress"))]
        return Ok(Self {
            filename,
            client,
            workers,
            url: parsed,
            hash: None,
            length,
            chunks,
        });
        #[cfg(feature = "progress")]
        return Ok(Self {
            filename,
            client,
            workers,
            url: parsed,
            hash: None,
            length,
            chunks,
            pb: None,
        });
    }
    pub async fn new_manual(url: &str, workers: u8, length: u64) -> Result<Self> {
        let client = Client::new();
        Self::assemble_downloader(url, workers, length, client).await
    }
    /// Create a new downloader
    ///
    /// # Arguments
    /// * `url` - URL of the file
    /// * `workers` - amount of concurrent tasks
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use manic::Downloader;
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), manic::ManicError> {
    ///     // If only one TLS feature is enabled
    ///     let downloader = Downloader::new("https://crates.io", 5).await?;
    ///
    /// # Ok(())
    /// # }
    /// ```
    pub async fn new(url: &str, workers: u8) -> Result<Self> {
        let client = Client::new();
        let length = content_length(&client, url).await?;
        Self::assemble_downloader(url, workers, length, client).await
    }
    pub(crate) fn url_to_filename(url: &reqwest::Url) -> Result<String> {
        url.path_segments()
            .and_then(|segments| segments.last())
            .and_then(|name| {
                if name.is_empty() {
                    None
                } else {
                    Some(name.to_string())
                }
            })
            .ok_or_else(|| ManicError::NoFilename(url.to_string()))
    }
    /// Enable progress reporting
    #[cfg(feature = "progress")]
    pub fn progress_bar(&mut self) -> &mut Self {
        self.pb = Some(indicatif::ProgressBar::new(self.length));
        self
    }
    #[cfg(feature = "progress")]
    pub fn connect_progress(&mut self, pb: ProgressBar) {
        self.pb = Some(pb);
    }
    /// Set the progress bar style
    #[cfg(feature = "progress")]
    pub fn bar_style(&self, style: indicatif::ProgressStyle) {
        if let Some(pb) = &self.pb {
            pb.set_style(style);
        }
    }
    /// Add a SHA checksum to verify against
    /// # Arguments
    /// * `hash` - [`Hash`][crate::Hash] to verify against
    pub fn verify(&mut self, hash: Hash) -> Self {
        self.hash = Some(hash);
        self.to_owned()
    }
    #[instrument(skip(client, pb, val), fields(range = %val.bytes))]
    async fn download_chunk(
        client: Client,
        url: String,
        pb: Option<ProgressBar>,
        val: Chunk,
    ) -> Result<Chunk> {
        let mut resp = client
            .get(url.to_string())
            .header(RANGE, val.bytes.clone())
            .send()
            .await?;
        {
            let mut res = val.buf.lock().await;
            while let Some(chunk) = resp.chunk().await? {
                #[cfg(feature = "progress")]
                if let Some(bar) = &pb {
                    bar.inc(chunk.len() as u64);
                }
                res.append(&mut chunk.to_vec());
            }
            Ok(val.clone())
        }
    }
    /// Download the file and verify if hash is set
    ///
    /// # Example
    ///
    /// ```no_run
    /// use manic::Downloader;
    /// use manic::ManicError;
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), ManicError> {
    /// let client = Downloader::new("https://crates.io", 5).await?;
    /// let result = client.download().await?;
    /// # Ok(())
    /// # }
    /// ```
    #[instrument(skip(self), fields(URL=%self.url, tasks=%self.workers))]
    pub async fn download(&self) -> Result<ChunkVec> {
        let mb = &self.length / 1000000;
        debug!("File size: {}MB", mb);
        let chnks = self.chunks;
        let url = self.url.clone();
        let client = self.client.clone();
        #[cfg(feature = "progress")]
        let pb = self.pb.clone();
        let hndl_vec = chnks
            .into_iter()
            .map(move |x| {
                tokio::spawn(Self::download_chunk(
                    client.clone(),
                    url.clone().to_string(),
                    #[cfg(feature = "progress")]
                    pb.clone(),
                    x,
                ))
            })
            .collect::<Vec<_>>();
        let mut res: Vec<Chunk> = join_all(hndl_vec).await?;
        res.sort_unstable_by(|a, b| a.pos.cmp(&b.pos));
        let result: Vec<u8> = {
            let mut result = Vec::new();
            for i in &res {
                result.append(&mut i.buf.lock().await.to_vec());
            }
            result
        };
        if let Some(hash) = &self.hash {
            hash.verify(&result)?;
            debug!("Compared");
        }
        ChunkVec::from_vec(res).await
    }
    pub(crate) async fn multi_download(self) -> Result<Downloaded> {
        let res = self.download().await?;
        Ok(Downloaded::new(
            self.get_url(),
            self.filename,
            res.as_vec().await,
        ))
    }
    /// Used to download, save to a file and verify against a SHA256 sum,
    /// returns an error if the connection fails or if the sum doesn't match the one provided
    ///
    /// # Arguments
    /// * `path` - path to save the file to, if it's a directory then the original filename is used
    /// * `verify` - set true to verify the file against the hash
    ///
    /// # Example
    ///
    /// ```no_run
    /// use manic::Downloader;
    /// use manic::ManicError;
    /// use manic::Hash;
    /// #[tokio::main]
    /// async fn main() -> Result<(), ManicError> {
    ///     let hash = Hash::SHA256("039058c6f2c0cb492c533b0a4d14ef77cc0f78abccced5287d84a1a2011cfb81".to_string());
    ///     let client = Downloader::new("https://crates.io", 5).await?.verify(hash);
    ///     client.download_and_save("~/Downloads").await?;
    ///     Ok(())
    ///  }
    /// ```
    ///
    #[instrument(skip(self))]
    pub async fn download_and_save(&self, path: &str) -> Result<()> {
        let mut result = {
            let original_path = Path::new(path);
            let file_path = if original_path.is_dir() {
                original_path.join(&self.filename)
            } else {
                original_path.to_path_buf()
            };
            File::create(file_path).await?
        };
        let data = self.download().await?;
        let c = result.try_clone().await?;
        data.save(c).await?;
        result.sync_all().await?;
        result.flush().await?;
        Ok(())
    }
}

#[instrument(skip(client, url), fields(URL=%url))]
async fn content_length(client: &Client, url: &str) -> Result<u64> {
    let resp = client.head(url).send().await?;
    debug!("Response code: {}", resp.status());
    debug!("Received HEAD response: {:?}", resp.headers());
    let len = resp
        .headers()
        .get("content-length")
        .ok_or(ManicError::NoLen);
    if len.is_ok() && resp.status().is_success() {
        len?.to_str()
            .map_err(|_x| ManicError::NoLen)?
            .parse::<u64>()
            .map_err(|e| e.into())
    } else {
        let resp = client.get(url).header(RANGE, "0-0").send().await?;
        debug!("Response code: {}", resp.status());
        debug!("Received GET 1B response: {:?}", resp.headers());
        resp.headers()
            .get(CONTENT_LENGTH)
            .ok_or(ManicError::NoLen)?
            .to_str()?
            .parse::<u64>()
            .map_err(|e| e.into())
    }
}

pub(crate) async fn join_all<T: Clone>(i: Vec<JoinHandle<Result<T>>>) -> Result<Vec<T>> {
    let results = futures::future::join_all(i)
        .await
        .iter_mut()
        .filter_map(|x| x.as_ref().ok())
        .cloned()
        .collect::<Vec<_>>();
    let errs = results
        .iter()
        .cloned()
        .filter_map(|x| x.err())
        .collect::<Vec<_>>();
    let successful = results
        .iter()
        .filter_map(|x| x.as_ref().ok())
        .cloned()
        .collect::<Vec<T>>();
    if !errs.is_empty() && successful.is_empty() {
        Err(errs.into())
    } else if !successful.is_empty() {
        Ok(successful)
    } else {
        Err(ManicError::NoResults)
    }
}
