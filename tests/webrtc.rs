// Copyright 2023 litep2p developers
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

#![cfg(feature = "webrtc")]

use futures::StreamExt;
use litep2p::{
    config::ConfigBuilder as Litep2pConfigBuilder,
    crypto::ed25519::Keypair,
    protocol::{libp2p::ping, notification::ConfigBuilder},
    transport::webrtc::config::Config,
    types::protocol::ProtocolName,
    Litep2p,
};
use multiaddr::Protocol;

#[tokio::test]
#[ignore]
async fn webrtc_test() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (ping_config, mut ping_event_stream) = ping::Config::default();
    let (notif_config, mut notif_event_stream) = ConfigBuilder::new(ProtocolName::from(
        // Westend block-announces protocol name.
        "/e143f23803ac50e8f6f8e62695d1ce9e4e1d68aa36c1cd2cfd15340213f3423e/block-announces/1",
    ))
    .with_max_size(5 * 1024 * 1024)
    .with_handshake(vec![1, 2, 3, 4])
    .with_auto_accept_inbound(true)
    .build();

    let config = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_webrtc(Config {
            listen_addresses: vec!["/ip4/192.168.1.170/udp/8888/webrtc-direct".parse().unwrap()],
            ..Default::default()
        })
        .with_libp2p_ping(ping_config)
        .with_notification_protocol(notif_config)
        .build();

    let mut litep2p = Litep2p::new(config).unwrap();
    let address = litep2p.listen_addresses().next().unwrap().clone();

    tracing::info!("listen address: {address:?}");

    loop {
        tokio::select! {
            event = litep2p.next_event() => {
                tracing::debug!("litep2p event received: {event:?}");
            }
            event = ping_event_stream.next() => {
                if std::matches!(event, None) {
                    tracing::error!("ping event stream terminated");
                    break
                }
                tracing::error!("ping event received: {event:?}");
            }
            _event = notif_event_stream.next() => {
                // tracing::error!("notification event received: {event:?}");
            }
        }
    }
}

#[tokio::test]
async fn unspecified_listen_address_webrtc() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    // Test IPv4 unspecified address
    let config_ipv4 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_webrtc(Config {
            listen_addresses: vec!["/ip4/0.0.0.0/udp/0/webrtc-direct".parse().unwrap()],
            ..Default::default()
        })
        .build();

    let litep2p_ipv4 = Litep2p::new(config_ipv4).unwrap();
    let listen_addresses_ipv4: Vec<_> = litep2p_ipv4.listen_addresses().cloned().collect();

    // Verify we have at least one address (should be multiple if interfaces exist)
    assert!(!listen_addresses_ipv4.is_empty(), "should have at least one listen address");

    // Verify none of the addresses contain 0.0.0.0
    for address in &listen_addresses_ipv4 {
        let mut iter = address.iter();
        match iter.next() {
            Some(Protocol::Ip4(ip)) => {
                assert!(!ip.is_unspecified(), "should not contain 0.0.0.0 in listen addresses");
            }
            _ => panic!("expected IPv4 protocol"),
        }
    }

    // Verify all addresses have the same port
    let ports: Vec<_> = listen_addresses_ipv4
        .iter()
        .filter_map(|address| {
            address.iter().find_map(|p| match p {
                Protocol::Udp(port) => Some(port),
                _ => None,
            })
        })
        .collect();

    if ports.len() > 1 {
        let first_port = ports[0];
        assert!(
            ports.iter().all(|&p| p == first_port),
            "all addresses should have the same port"
        );
    }

    // Verify all addresses have WebRTC protocol
    for address in &listen_addresses_ipv4 {
        let has_webrtc = address.iter().any(|p| matches!(p, Protocol::WebRTC));
        assert!(has_webrtc, "address should contain WebRTC protocol: {}", address);
    }

    // Verify all addresses have Certhash protocol
    for address in &listen_addresses_ipv4 {
        let has_certhash = address.iter().any(|p| matches!(p, Protocol::Certhash(_)));
        assert!(has_certhash, "address should contain Certhash protocol: {}", address);
    }

    // Test IPv6 unspecified address
    let config_ipv6 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_webrtc(Config {
            listen_addresses: vec!["/ip6/::/udp/0/webrtc-direct".parse().unwrap()],
            ..Default::default()
        })
        .build();

    let litep2p_ipv6 = Litep2p::new(config_ipv6).unwrap();
    let listen_addresses_ipv6: Vec<_> = litep2p_ipv6.listen_addresses().cloned().collect();

    // Verify we have at least one address (should be multiple if interfaces exist)
    assert!(!listen_addresses_ipv6.is_empty(), "should have at least one listen address");

    // Verify none of the addresses contain :: or link-local addresses (fe80::)
    for address in &listen_addresses_ipv6 {
        let mut iter = address.iter();
        match iter.next() {
            Some(Protocol::Ip6(ip)) => {
                assert!(!ip.is_unspecified(), "should not contain :: in listen addresses");
                // Verify no link-local addresses (fe80::/10)
                assert_ne!(
                    ip.segments()[0],
                    0xfe80,
                    "should not contain link-local addresses (fe80::)"
                );
            }
            _ => panic!("expected IPv6 protocol"),
        }
    }

    // Verify all addresses have the same port
    let ports_ipv6: Vec<_> = listen_addresses_ipv6
        .iter()
        .filter_map(|address| {
            address.iter().find_map(|p| match p {
                Protocol::Udp(port) => Some(port),
                _ => None,
            })
        })
        .collect();

    if ports_ipv6.len() > 1 {
        let first_port = ports_ipv6[0];
        assert!(
            ports_ipv6.iter().all(|&p| p == first_port),
            "all addresses should have the same port"
        );
    }

    // Verify all addresses have WebRTC protocol
    for address in &listen_addresses_ipv6 {
        let has_webrtc = address.iter().any(|p| matches!(p, Protocol::WebRTC));
        assert!(has_webrtc, "address should contain WebRTC protocol: {}", address);
    }

    // Verify all addresses have Certhash protocol
    for address in &listen_addresses_ipv6 {
        let has_certhash = address.iter().any(|p| matches!(p, Protocol::Certhash(_)));
        assert!(has_certhash, "address should contain Certhash protocol: {}", address);
    }
}
