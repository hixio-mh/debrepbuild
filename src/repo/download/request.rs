use checksum::hasher;
use reqwest::Client;
use sha2::Sha256;
use std::{fs::{self, File}, io};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use utime;

const ATTEMPTS: u8 = 3;

pub enum RequestCompare<'a> {
    Checksum(Option<&'a str>),
    SizeAndModification(u64, Option<i64>)
}

pub fn file(client: &Client, url: &str, compare: RequestCompare, path: &Path) -> io::Result<u64> {
    let mut tries = 0;
    loop {
        let mut file = if path.exists() {
            let mut requires_download = true;

            match compare {
                RequestCompare::Checksum(Some(checksum)) => {
                    let digest = hasher::<Sha256, File>(File::open(path)?)?;
                    requires_download = digest != checksum;
                }
                RequestCompare::SizeAndModification(length, mtime) => {
                    let file = File::open(path)?;
                    let metadata = file.metadata()?;
                    if metadata.len() == length {
                        if let Some(modified) = mtime {
                            if modified == metadata.mtime() {
                                requires_download = false;
                            }
                        } else {
                            requires_download = false;
                        }
                    }
                }
                _ => ()
            }

            if ! requires_download {
                return Ok(0);
            }

            fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(path)?
        } else {
            if let Some(parent) = path.parent() {
                if !parent.exists() {
                    fs::create_dir_all(parent)?;
                }
            }
            File::create(path)?
        };

        info!("downloading package to {}", path.display());
        let downloaded = client
            .get(url)
            .send()
            .map_err(|why| io::Error::new(io::ErrorKind::Other, format!("reqwest get failed: {}", why)))?
            .copy_to(&mut file)
            .map_err(|why| io::Error::new(io::ErrorKind::Other, format!("reqwest copy failed: {}", why)))?;

        info!("finished downloading {}", path.display());
        if let RequestCompare::Checksum(Some(checksum)) = compare {
            let digest = hasher::<Sha256, File>(File::open(path)?)?;
            if digest == checksum {
                return Ok(downloaded);
            } else {
                error!("checksum does not much for {}, removing.", path.display());
                fs::remove_file(&path)?;

                if tries == ATTEMPTS {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("checksum does not match for {}", path.display())
                    ));
                }

                tries += 1;
            }
        } else if let RequestCompare::SizeAndModification(_length, Some(mtime)) = compare {
            let (atime, _) = utime::get_file_times(path)?;
            utime::set_file_times(path, atime, mtime as u64)?;
            return Ok(downloaded);
        } else {
            return Ok(downloaded);
        }
    }
}
