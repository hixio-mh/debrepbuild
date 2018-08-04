use ar;
use config::Config;
use libflate::gzip::Decoder as GzDecoder;
use misc;
use rayon::prelude::*;
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tar;
use xz2::read::XzDecoder;

use super::compress::*;

/// Generates the binary files from Debian packages that exist within the pool, using
/// `apt-ftparchive`
pub(crate) fn binary_files(config: &Config, dist_base: &str, suites: &[(String, PathBuf)]) -> io::Result<()> {
    info!("generating binary files");
    suites.par_iter().map(|&(ref arch, ref path)| {
        info!("generating binary files for {}, from {}", arch, path.display());
        let out_path: &Path = &Path::new(dist_base).join("main").join(arch);

        fs::create_dir_all(path)?;
        fs::create_dir_all(out_path)?;

        let arch = match arch.as_str() {
            "amd64" => "binary-amd64",
            "i386" => "binary-i386",
            "all" => "binary-all",
            arch => panic!("unsupported architecture: {}", arch),
        };

        Command::new("apt-ftparchive")
            .arg("packages")
            .arg(path)
            .stderr(Stdio::inherit())
            .stdout(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                {
                    let stdout = child.stdout.as_mut().unwrap();
                    compress("Packages", out_path, stdout, UNCOMPRESSED | GZ_COMPRESS | XZ_COMPRESS)?;
                }
                
                child.wait().and_then(|stat| {
                    if stat.success() {
                        Ok(())
                    } else {
                        Err(io::Error::new(io::ErrorKind::Other, "apt-ftparchive failed"))
                    }
                })
            })?;

        let mut release = File::create(out_path.join("Release"))?;
        writeln!(&mut release, "Archive: {}", config.archive)?;
        writeln!(&mut release, "Version: {}", config.version)?;
        writeln!(&mut release, "Component: main")?;
        writeln!(&mut release, "Origin: {}", config.origin)?;
        writeln!(&mut release, "Label: {}", config.label)?;
        writeln!(&mut release, "Architecture: {}", arch)
    }).collect()
}

pub(crate) fn sources_index(dist_base: &str, pool_base: &str) -> io::Result<()> {
    info!("generating sources index");
    let path = PathBuf::from([dist_base, "/main/source/"].concat());
    fs::create_dir_all(&path)?;

    Command::new("apt-ftparchive")
        .arg("sources")
        .arg(PathBuf::from(pool_base).join("source"))
        .stderr(Stdio::inherit())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            {
                let stdout = child.stdout.as_mut().unwrap();
                compress("Sources", &path, stdout, UNCOMPRESSED | GZ_COMPRESS | XZ_COMPRESS)?;
            }
            
            child.wait().and_then(|stat| {
                if stat.success() {
                    Ok(())
                } else {
                    Err(io::Error::new(io::ErrorKind::Other, "apt-ftparchive failed"))
                }
            })
        })
}

/// Generates the dists release file via `apt-ftparchive`.
pub(crate) fn dists_release(config: &Config, base: &str) -> io::Result<()> {
    info!("generating dists release files");

    let cwd = env::current_dir()?;
    env::set_current_dir(base)?;

    let release = Command::new("apt-ftparchive")
        .arg("-o")
        .arg(format!(
            "APT::FTPArchive::Release::Origin={}",
            config.origin
        ))
        .arg("-o")
        .arg(format!("APT::FTPArchive::Release::Label={}", config.label))
        .arg("-o")
        .arg(format!(
            "APT::FTPArchive::Release::Suite={}",
            config.archive
        ))
        .arg("-o")
        .arg(format!(
            "APT::FTPArchive::Release::Version={}",
            config.version
        ))
        .arg("-o")
        .arg(format!(
            "APT::FTPArchive::Release::Codename={}",
            config.archive
        ))
        .arg("-o")
        .arg("APT::FTPArchive::Release::Architectures=i386 amd64 all")
        .arg("-o")
        .arg("APT::FTPArchive::Release::Components=main")
        .arg("-o")
        .arg(format!(
            "APT::FTPArchive::Release::Description={} ({} {})",
            config.label, config.archive, config.version
        ))
        .arg("release")
        .arg(".")
        .output()
        .map(|data| data.stdout)?;

    let mut release_file = File::create("Release")?;
    release_file.write_all(&release)?;
    env::set_current_dir(cwd)
}

/// Generates the `InRelease` file from the `Release` file via `gpg --clearsign`.
pub(crate) fn gpg_in_release(email: &str, release_path: &Path, out_path: &Path) -> io::Result<()> {
    info!("generating InRelease file");
    let exit_status = Command::new("gpg")
        .args(&[
            "--clearsign",
            "--local-user",
            email,
            "--batch",
            "--yes",
            "--digest-algo",
            "sha512",
            "-o",
        ])
        .arg(out_path)
        .arg(release_path)
        .status()?;

    if exit_status.success() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "gpg_in_release failed",
        ))
    }
}

/// Generates the `Release.gpg` file from the `Release` file via `gpg -abs`
pub(crate) fn gpg_release(email: &str, release_path: &Path, out_path: &Path) -> io::Result<()> {
    info!("generating Release.gpg file");
    let exit_status = Command::new("gpg")
        .args(&[
            "-abs",
            "--local-user",
            email,
            "--batch",
            "--yes",
            "--digest-algo",
            "sha512",
            "-o",
        ])
        .arg(out_path)
        .arg(release_path)
        .status()?;

    if exit_status.success() {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::Other, "gpg_release failed"))
    }
}

struct ContentIterator<T> {
    content: T,
}

impl<T: Iterator<Item = (PathBuf, String)>> Iterator for ContentIterator<T> {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        let (path, package) = self.content.next()?;
        let path = path.as_os_str().as_bytes();
        let mut serialized = Vec::new();
        serialized.extend_from_slice(if &path[..2] == b"./" {
            &path[2..]
        } else {
            path
        });
        serialized.extend_from_slice(b"  ");
        serialized.extend_from_slice(package.as_bytes());
        serialized.push(b'\n');
        Some(serialized)
    }
}

struct ContentReader<T> {
    buffer: Vec<u8>,
    data: T
}

impl<T: Iterator<Item = Vec<u8>>> io::Read for ContentReader<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.buffer.is_empty() {
            while self.buffer.len() < buf.len() {
                match self.data.next() {
                    Some(data) => self.buffer.extend_from_slice(&data),
                    None => break
                }
            }

            if self.buffer.is_empty() {
                return Ok(0);
            }
        }

        let to_write = self.buffer.len().min(buf.len());
        buf[..to_write].copy_from_slice(&self.buffer[..to_write]);
        if to_write != self.buffer.len() {
            let leftovers = self.buffer.len() - to_write;
            if self.buffer.capacity() < leftovers {
                let reserve = self.buffer.capacity() - leftovers;
                self.buffer.reserve_exact(reserve);
            }

            for (new, old) in (to_write..self.buffer.len()).enumerate() {
                self.buffer[new] = self.buffer[old];
            }

            self.buffer.truncate(leftovers);
        } else {
            self.buffer.clear();
        }

        Ok(to_write)
    }
}

enum DecoderVariant {
    Xz,
    Gz,
}

struct ContentsEntry {
    package: String,
    files: Vec<PathBuf>
}

pub(crate) fn contents(dist_base: &str, suites: &[(String, PathBuf)]) -> io::Result<()> {
    info!("generating content archives");
    let branch_name = "main";
    
    suites.par_iter().map(|&(ref arch, ref path)| {
        // Collects a list of deb packages to read, and then reads them in parallel.
        let entries: Vec<io::Result<ContentsEntry>> = misc::walk_debs(&path)
            .filter(|e| !e.file_type().is_dir())
            .map(|e| e.path().to_path_buf())
            .collect::<Vec<PathBuf>>()
            .into_par_iter()
            .map(|debian_entry| {
                let mut files = Vec::new();
                info!("processing contents of {:?}", debian_entry);
                let mut archive = ar::Archive::new(File::open(&debian_entry)?);

                let mut control = None;
                let mut data = None;
                let mut entry_id = 0;
                let package_name: String;

                while let Some(entry_result) = archive.next_entry() {
                    if let Ok(mut entry) = entry_result {
                        match entry.header().identifier() {
                            b"data.tar.xz" => data = Some((entry_id, DecoderVariant::Xz)),
                            b"data.tar.gz" => data = Some((entry_id, DecoderVariant::Gz)),
                            b"control.tar.xz" => control = Some((entry_id, DecoderVariant::Xz)),
                            b"control.tar.gz" => control = Some((entry_id, DecoderVariant::Gz)),
                            _ => {
                                entry_id += 1;
                                continue
                            }
                        }

                        if data.is_some() && control.is_some() { break }
                    }

                    entry_id += 1;
                }

                drop(archive);

                if let (Some((data, data_codec)), Some((control, control_codec))) = (data, control) {
                    let mut package = None;
                    let mut section = None;

                    {
                        let mut archive = ar::Archive::new(File::open(&debian_entry)?);
                        let control = archive.jump_to_entry(control)?;
                        let mut reader: Box<io::Read> = match control_codec {
                            DecoderVariant::Xz => Box::new(XzDecoder::new(control)),
                            DecoderVariant::Gz => Box::new(GzDecoder::new(control)?)
                        };

                        let control_file = Path::new("./control");

                        for mut entry in tar::Archive::new(reader).entries()? {
                            let mut entry = entry?;
                            let path = entry.path()?.to_path_buf();
                            if &path == control_file {
                                for line in BufReader::new(&mut entry).lines() {
                                    let line = line?;
                                    if line.starts_with("Package:") {
                                        package = Some(line[8..].trim().to_owned());
                                    } else if line.starts_with("Section:") {
                                        section = Some(line[8..].trim().to_owned());
                                    }

                                    if package.is_some() && section.is_some() { break }
                                }
                            }
                        }
                    }

                    package_name = match (package, section) {
                        (Some(ref package), Some(ref section)) if branch_name == "main" => [section, "/", package].concat(),
                        (Some(ref package), Some(ref section)) => [branch_name, "/", section, "/", package].concat(),
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::Other,
                                "did not find package + section from control archive"
                            ));
                        }
                    };

                    let mut archive = ar::Archive::new(File::open(&debian_entry)?);
                    let data = archive.jump_to_entry(data)?;
                    let mut reader: Box<io::Read> = match data_codec {
                        DecoderVariant::Xz => Box::new(XzDecoder::new(data)),
                        DecoderVariant::Gz => Box::new(GzDecoder::new(data)?)
                    };

                    for entry in tar::Archive::new(reader).entries()? {
                        let entry = entry?;
                        if entry.header().entry_type().is_dir() {
                            continue
                        }

                        let path = entry.path()?;
                        files.push(path.to_path_buf());
                    }
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "could not find data + control entries in deb archive"
                    ));
                }

                Ok(ContentsEntry { package: package_name, files })
            }).collect();

        // Mux the files together, and sort the entries by paths.
        let file_map = {
            let mut combined_capacity = 0;
            let mut packages = Vec::with_capacity(entries.len());
            for entry in entries {
                let entry = entry?;
                combined_capacity += entry.files.len();
                packages.push(entry);
            }

            let mut file_map = Vec::with_capacity(combined_capacity);
            
            for entry in packages {
                for path in entry.files {
                    file_map.push((path, entry.package.clone()));
                }
            }

            file_map.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            file_map
        };

        // Check for duplicate entries, and error if found.
        file_map.windows(2)
            .position(|window| window[0] == window[1])
            .map_or(Ok(()), |pos| {
                let a = &file_map[pos];
                let b = &file_map[pos+1];
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("{} and {} both have {}", a.1, b.1, a.0.display())
                ))
            })?;

        let reader = ContentReader {
            buffer: Vec::with_capacity(64 * 1024),
            data: ContentIterator {
                content: file_map.into_iter()
            }
        };

        compress(&["Contents-", &arch].concat(), &Path::new(dist_base), reader, GZ_COMPRESS | XZ_COMPRESS)
    }).collect()
}