use monoio::io::stream::Stream;
use monoio_quiche::Connection;
use quiche::ConnectionId;
use tracing_subscriber::prelude::*;

const MAX_DATAGRAM_SIZE: usize = 1350;

#[monoio::main(timer_enabled = true)]
async fn main() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    config.verify_peer(false);
    config
        .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
        .unwrap();
    config.set_max_idle_timeout(50000);
    config.set_max_recv_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_max_send_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_stream_data_uni(1_000_000);
    config.set_initial_max_streams_bidi(100);
    config.set_initial_max_streams_uni(100);
    config.set_disable_active_migration(true);

    let mut stream = Connection::connect(
        "ihc.im:443",
        Some("ihc.im"),
        &ConnectionId::from_ref(&[0; 20]),
        &mut config,
    )
    .await
    .unwrap();
    tracing::info!("handshake success!");

    let h3_config = quiche::h3::Config::new().unwrap();
    let mut h3 = stream.h3(&h3_config).unwrap();

    let req = vec![
        quiche::h3::Header::new(b":method", b"GET"),
        quiche::h3::Header::new(b":scheme", b"https"),
        quiche::h3::Header::new(b":authority", b"ihc.im"),
        quiche::h3::Header::new(b":path", b"/"),
        quiche::h3::Header::new(b"user-agent", b"monoio-quiche"),
    ];
    let stream_id = h3.send_request(&req, true).await.unwrap();
    tracing::info!("h3 request sent with stream {stream_id}");

    let mut body = Vec::new();
    loop {
        let res = h3.next().await.unwrap();
        match res {
            Ok((stream_id, quiche::h3::Event::Data)) => {
                let mut buf = [0; 2048];
                while let Ok(read) = h3.recv_body(stream_id, &mut buf) {
                    tracing::info!("h3 recv {read} bytes of response data on {stream_id}",);
                    body.extend_from_slice(&buf[..read]);
                }
            }
            Ok((stream_id, quiche::h3::Event::Headers { list, has_body })) => {
                tracing::info!(
                    "h3 event on {stream_id}: {:?}",
                    quiche::h3::Event::Headers { list, has_body }
                );
            }
            Ok((stream_id, quiche::h3::Event::Finished)) => {
                tracing::info!("h3 finished on {stream_id}");
                stream.close(true, 0x00, b"kthxbye").await.unwrap();
                break;
            }

            Ok((_, quiche::h3::Event::Reset(_))) => {
                tracing::info!("h3 reset");
                stream.close(true, 0x00, b"kthxbye").await.unwrap();
                break;
            }
            Ok((_, event)) => {
                tracing::info!("h3 event: {event:?}");
            }
            Err(e) => {
                tracing::error!("h3 failed: {e:?}");
                return;
            }
        }
    }
    print!("{}", String::from_utf8_lossy(&body));
}
