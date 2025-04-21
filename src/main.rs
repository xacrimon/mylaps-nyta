use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufStream};
use tokio::select;
use tokio::sync::mpsc;
use tokio::{
    fs,
    net::{TcpListener, TcpStream},
};

#[derive(Debug, Parser)]
#[command(name = "mylaps-nyta")]
#[command(about = "MyLaps TCP/IP exporter till NytaTiming", long_about = None)]
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
    let config: Config = toml::from_str(&config_data).context("failed to parse config file")?;

    let bind_addr = format!("{}:{}", config.host, config.port);
    let listener = TcpListener::bind(&bind_addr).await?;
    log::info!("listening on {}", bind_addr);

    let (time_tx, mut time_rx) = mpsc::channel(1000);
    let (signal_tx, mut signal_rx) = mpsc::channel(1);
    ctrlc::set_handler(move || signal_tx.blocking_send(()).unwrap())
        .context("error setting Ctrl-C handler")?;

    tokio::spawn(async move {
        loop {
            if let Err(e) = push_passings_from(&mut time_rx, &config).await {
                log::error!(
                    "fatal pushing passings: {}, restarting export API worker",
                    e
                );
            }
        }
    });

    loop {
        select! {
            _ = signal_rx.recv() => {
                log::info!("received Ctrl-C, shutting down");
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

async fn handle_connection(socket: TcpStream, mut time_tx: mpsc::Sender<String>) -> Result<()> {
    let mut socket = BufStream::new(socket);

    'outer: loop {
        let mut line = String::new();
        let mut b = [0u8; 1];

        'inner: loop {
            match socket.read(&mut b).await {
                Ok(0) => break 'outer,
                Ok(1) => {
                    if b[0] == b'$' {
                        let sub = line.trim();
                        process_line(&mut socket, sub, &mut time_tx).await?;
                        line.clear();
                        break 'inner;
                    }

                    line.push(b[0] as char);
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
    socket: &mut BufStream<TcpStream>,
    line: &str,
    time_tx: &mut mpsc::Sender<String>,
) -> Result<()> {
    log::info!("received line: {}", line);
    let parts: Vec<&str> = line.split('@').collect();

    let [_, cmd, data @ ..] = &parts[..] else {
        bail!("invalid command");
    };

    match *cmd {
        "Ping" => socket.write_all(b"NytaTiming@AckPing@$").await?,
        "Pong" => socket.write_all(b"NytaTiming@AckPong@Version2.1@$").await?,
        "Passing" => {
            let [records @ .., num, _] = data else {
                bail!("invalid passing command");
            };

            let num = num.parse::<u32>()?;
            for record in records {
                time_tx.send(record.to_string()).await?;
            }

            let reply = format!("NytaTiming@AckPassing@{}@$", num);
            socket.write_all(reply.as_bytes()).await?;
        }
        _ => {
            log::warn!("unknown command: {}", cmd);
        }
    }

    socket.flush().await?;
    Ok(())
}

async fn push_passings_from(time_rx: &mut mpsc::Receiver<String>, config: &Config) -> Result<()> {
    let client = reqwest::Client::new();

    while let Some(record) = time_rx.recv().await {
        let mut map = HashMap::new();
        for pair in record.split('|') {
            let mut parts = pair.split('=');
            let key = parts
                .next()
                .ok_or_else(|| anyhow!("received record missing key"))?;
            let value = parts
                .next()
                .ok_or_else(|| anyhow!("received record missing value"))?;

            map.insert(key, value);
        }

        let mut chip: String = map
            .get("c")
            .ok_or_else(|| anyhow!("missing chip"))?
            .to_string();

        chip.insert(2, '-');

        loop {
            match push_passing_single(&client, &chip, 0, config).await {
                Ok(_) => break,
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
    client: &reqwest::Client,
    chip: &str,
    time: i64,
    config: &Config,
) -> Result<()> {
    let uri = format!("https://nytatime.se{}", config.nyta_uri);
    client
        .get(&uri)
        .query(&[(("bib", chip), "time", time)])
        .send()
        .await?
        .error_for_status()?;

    log::info!("pushed passing for chip {} to NytaTiming", chip,);
    Ok(())
}
