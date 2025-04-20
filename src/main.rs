use anyhow::{Error, Result, anyhow, bail};
use clap::Parser;
use core::num;
use serde::Deserialize;
use std::collections::HashMap;
use std::os::unix::raw::time_t;
use std::sync::LazyLock;
use std::{path::PathBuf, sync::Arc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::select;
use tokio::sync::{mpsc, oneshot};
use tokio::{
    fs,
    net::{TcpListener, TcpStream},
};

#[derive(Debug, Parser)] // requires `derive` feature
#[command(name = "mylaps-nyta")]
#[command(about = "Läs in tider från MyLaps till NytaTiming", long_about = None)]
struct Cli {
    #[arg(short, long)]
    config: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Config {
    host: String,
    port: u16,
    nyta_uri: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();
    env_logger::init();

    let config_data = fs::read_to_string(&args.config).await?;
    let config: Config =
        toml::from_str(&config_data).map_err(|e| anyhow!("failed to parse config file: {}", e))?;

    let bind_addr = format!("{}:{}", config.host, config.port);
    let listener = TcpListener::bind(&bind_addr).await?;

    log::info!("listening on {}", bind_addr);

    let (signal_tx, signal_rx) = oneshot::channel();
    let mut signal_tx = Some(signal_tx);
    let mut signal_rx = Some(signal_rx);

    ctrlc::set_handler(move || signal_tx.take().unwrap().send(()).unwrap())
        .expect("error setting Ctrl-C handler");

    let (time_tx, time_rx) = mpsc::channel(100);

    tokio::spawn(async move {
        if let Err(e) = push_passings_from(time_rx, &config).await {
            log::error!("error pushing passings: {}", e);
        }
    });

    loop {
        select! {
            _ = signal_rx.take().unwrap() => {
                log::info!("received Ctrl-C, shutting down...");
                break;
            }

            Ok((socket, addr)) = listener.accept() => {
                log::info!("accepted connection from {}", addr);

                let time_tx = time_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(socket, time_tx).await {
                        log::error!("error handling connection: {}", e);
                    }
                });
            }
        }
    }

    Ok(())
}

async fn handle_connection(mut socket: TcpStream, mut time_tx: mpsc::Sender<String>) -> Result<()> {
    'outer: loop {
        let mut line = String::new();
        let mut b = [0u8; 1];

        loop {
            match socket.read(&mut b).await {
                Ok(0) => break 'outer,
                Ok(1) => {
                    if b[0] == b'$' {
                        break;
                    }

                    line.push(b[0] as char);
                    let sub = line.trim();
                    process_line(&mut socket, sub, &mut time_tx).await?;
                    line.clear();
                }
                Err(e) => bail!("error reading from socket: {}", e),
                _ => unreachable!(),
            }
        }
    }

    log::info!("connection closed gracefully");
    Ok(())
}

async fn process_line(
    socket: &mut TcpStream,
    line: &str,
    time_tx: &mut mpsc::Sender<String>,
) -> Result<()> {
    log::info!("received line: {}", line);
    let parts = line.split('@').collect::<Vec<_>>();

    assert!(parts.last() == Some(&"$"));
    let [_, cmd, data @ ..] = &parts[..] else {
        bail!("invalid command");
    };

    match *cmd {
        "Ping" => {
            socket.write_all(b"Nyta@AckPing@$").await?;
        }
        "Pong" => {
            socket.write_all(b"Nyta@AckPong@Version2.1@$").await?;
        }
        "Passing" => {
            let [records @ .., num] = data else {
                bail!("invalid store command");
            };

            dbg!(records, num);
            let num = num.parse::<u32>()?;

            for record in records {
                time_tx.send(record.to_string()).await?;
            }

            let reply = format!("Nyta@AckPassing@{}@$", num);
            socket.write_all(reply.as_bytes()).await?;
        }
        _ => {
            log::warn!("unknown command: {}", cmd);
            return Ok(());
        }
    }

    Ok(())
}

async fn push_passings_from(mut time_rx: mpsc::Receiver<String>, config: &Config) -> Result<()> {
    let client = reqwest::Client::new();

    while let Some(record) = time_rx.recv().await {
        let mut map = HashMap::new();
        for pair in record.split('|') {
            let mut parts = pair.split('=');
            let key = parts.next().unwrap();
            let value = parts.next().unwrap();

            map.insert(key, value);
        }

        dbg!(&map);
        let bib: u32 = map
            .get("b")
            .ok_or_else(|| anyhow!("missing bib"))?
            .parse()?;

        loop {
            match push_passing_single(&client, bib, config).await {
                Ok(_) => continue,
                Err(e) => {
                    log::error!("failed to push passing: {}", e);
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                }
            }
        }
    }

    log::info!("finished processing time records");
    Ok(())
}

async fn push_passing_single(
    client: &reqwest::Client, /* , time: i64*/
    bib: u32,
    config: &Config,
) -> Result<()> {
    let uri = format!("https://nytatime.se{}", config.nyta_uri);
    client
        .get(&uri)
        //.query(&[(("bib", bib), "time", time)])
        .query(&[(("bib", bib))])
        .send()
        .await?
        .error_for_status()?;

    log::info!("pushed passing for bib {} to NytaTiming", bib,);
    Ok(())
}
