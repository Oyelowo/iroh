use std::{net::SocketAddr, path::PathBuf};

use anyhow::{anyhow, ensure, Context, Result};
use clap::{Parser, Subcommand};
use console::style;
use futures::StreamExt;
use indicatif::{HumanDuration, ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use sendme::{client, server, PeerId};

#[derive(Parser, Debug, Clone)]
#[clap(version, about, long_about = None)]
#[clap(about = "Send data.")]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug, Clone)]
#[allow(clippy::large_enum_variant)]
enum Commands {
    /// Serve the data from the given path
    #[clap(about = "Serve the data from the given path")]
    Server {
        paths: Vec<PathBuf>,
        #[clap(long, short)]
        /// Optional port, defaults to 127.0.01:4433.
        addr: Option<SocketAddr>,
    },
    /// Fetch some data
    #[clap(about = "Fetch the data from the hash")]
    Client {
        hash: bao::Hash,
        #[clap(long)]
        /// PeerId of the server.
        peer_id: PeerId,
        #[clap(long, short)]
        /// Option address of the server, defaults to 127.0.0.1:4433.
        addr: Option<SocketAddr>,
        #[clap(long, short)]
        /// Option path to save the file, defaults to using the hash as the name.
        out: Option<PathBuf>,
    },
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Client {
            hash,
            peer_id,
            addr,
            out,
        } => {
            println!("Fetching: {}", hash.to_hex());
            let mut opts = client::Options {
                peer_id: Some(peer_id),
                ..Default::default()
            };
            if let Some(addr) = addr {
                opts.addr = addr;
            }

            // Write file out
            let outpath = out.unwrap_or_else(|| hash.to_string().into());

            println!("{} Connecting ...", style("[1/3]").bold().dim());
            let pb = ProgressBar::hidden();
            let stream = client::run(hash, opts);
            tokio::pin!(stream);
            while let Some(event) = stream.next().await {
                match event? {
                    client::Event::Connected => {
                        println!("{} Requesting ...", style("[2/3]").bold().dim());
                    }
                    client::Event::Requested { size } => {
                        println!("{} Downloading ...", style("[3/3]").bold().dim());
                        pb.set_style(
                            ProgressStyle::with_template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({eta})")
                                .unwrap()
                                .with_key("eta", |state: &ProgressState, w: &mut dyn std::fmt::Write| write!(w, "{:.1}s", state.eta().as_secs_f64()).unwrap())
                                .progress_chars("#>-")
                        );
                        pb.set_length(size as u64);
                        pb.set_draw_target(ProgressDrawTarget::stderr());
                    }
                    client::Event::Receiving {
                        hash: new_hash,
                        mut reader,
                    } => {
                        ensure!(hash == new_hash, "invalid hash received");
                        let parent = outpath
                            .parent()
                            .map(ToOwned::to_owned)
                            .ok_or_else(|| anyhow!("No valid parent directory for output file"))?;
                        let (temp_file, dup) = tokio::task::spawn_blocking(|| {
                            let temp_file = tempfile::Builder::new()
                                .prefix("sendme-tmp-")
                                .tempfile_in(parent)
                                .context("Failed to create temporary output file")?;
                            let dup = temp_file.as_file().try_clone()?;
                            Ok::<_, anyhow::Error>((temp_file, dup))
                        })
                        .await??;
                        let file = tokio::fs::File::from_std(dup);
                        let out = tokio::io::BufWriter::new(file);
                        // wrap for progress bar
                        let mut wrapped_out = pb.wrap_async_write(out);
                        tokio::io::copy(&mut reader, &mut wrapped_out).await?;
                        let outpath2 = outpath.clone();
                        tokio::task::spawn_blocking(|| temp_file.persist(outpath2))
                            .await?
                            .context("Failed to write output file")?;
                    }
                    client::Event::Done(stats) => {
                        pb.finish_and_clear();

                        println!("Done in {}", HumanDuration(stats.elapsed));
                    }
                }
            }
        }
        Commands::Server { paths, addr } => {
            let db = server::create_db(paths.iter().map(|p| p.as_path()).collect()).await?;
            let mut opts = server::Options::default();
            if let Some(addr) = addr {
                opts.addr = addr;
            }
            let mut server = server::Server::new(db);

            println!("Serving from {}", server.peer_id());
            server.run(opts).await?
        }
    }

    Ok(())
}
