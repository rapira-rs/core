use std::io;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::timeout;

use super::*;

#[tokio::test]
async fn stale_ready_event_waits_for_the_next_connection() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let drainer = listener.try_clone().unwrap();
    let listener = Listener::from_std(listener).unwrap();
    let first_client = std::net::TcpStream::connect(addr).unwrap();
    let (drained_tx, drained_rx) = oneshot::channel();
    let second_client = tokio::spawn(async move {
        drained_rx.await.unwrap();
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let local_addr = stream.local_addr().unwrap();
        (stream, local_addr)
    });

    let mut drained_tx = Some(drained_tx);
    let mut drained_stream = None;
    let mut calls = 0;
    let (_, peer) = timeout(
        Duration::from_secs(2),
        listener.accept_with(|registered| {
            calls += 1;
            if let Some(tx) = drained_tx.take() {
                drained_stream = Some(drainer.accept()?.0);
                let result = registered.accept();
                assert!(matches!(
                    &result,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock
                ));
                tx.send(()).unwrap();
                return result;
            }
            registered.accept()
        }),
    )
    .await
    .expect("accept must wait for the second connection")
    .unwrap();
    let (second_client, second_addr) = second_client.await.unwrap();

    assert_eq!(calls, 2);
    assert!(drained_stream.is_some());
    assert_eq!(peer, second_addr);
    drop((first_client, second_client));
}

#[tokio::test]
async fn missing_registration_makes_rotation_fail_with_enoent() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = Listener::from_std(listener).unwrap();
    listener
        .epoll
        .get_ref()
        .control(libc::EPOLL_CTL_DEL)
        .unwrap();

    let error = listener.on_accept().unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
}
