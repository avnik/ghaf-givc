use clap::Parser;
use givc::admin;
use givc::endpoint::TlsConfig;
use std::net::SocketAddr;
use std::path::PathBuf;
use tonic::transport::Server;
use tracing::info;

#[derive(Debug, Parser)] // requires `derive` feature
#[command(name = "givc-admin")]
#[command(about = "A givc admin", long_about = None)]
struct Cli {
    #[arg(long, env = "ADDR", default_missing_value = "127.0.0.1")]
    addr: String,
    #[arg(long, env = "PORT", default_missing_value = "9000")]
    port: u16,

    #[arg(long, env = "TLS", default_missing_value = "false")]
    use_tls: bool,

    #[arg(long, env = "CA_CERT")]
    ca_cert: Option<PathBuf>,

    #[arg(long, env = "HOST_CERT")]
    host_cert: Option<PathBuf>,

    #[arg(long, env = "HOST_KEY")]
    host_key: Option<PathBuf>,

    #[arg(
        long,
        env = "SERVICES",
        use_value_delimiter = true,
        value_delimiter = ','
    )]
    services: Option<Vec<String>>,
}

// FIXME: should be in src/lib.rs: mod pb {}, but doesn't work
mod kludge {
    pub const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("admin_descriptor");
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    givc::trace_init();

    let cli = Cli::parse();
    info!("CLI is {:#?}", cli);

    let addr = SocketAddr::new(cli.addr.parse().unwrap(), cli.port);

    let mut builder = Server::builder();

    let tls = if cli.use_tls {
        let tls = TlsConfig {
            ca_cert_file_path: cli.ca_cert.ok_or(String::from("required"))?,
            cert_file_path: cli.host_cert.ok_or(String::from("required"))?,
            key_file_path: cli.host_key,
        };
        let tls_config = tls.server_config()?;
        builder = builder.tls_config(tls_config)?;
        Some(tls)
    } else {
        None
    };

    let reflect = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(kludge::FILE_DESCRIPTOR_SET)
        .build()
        .unwrap();

    let admin_service_svc =
        admin::server::AdminServiceServer::new(admin::server::AdminService::new(tls));

    builder
        .add_service(reflect)
        .add_service(admin_service_svc)
        .serve(addr)
        .await?;

    Ok(())
}
