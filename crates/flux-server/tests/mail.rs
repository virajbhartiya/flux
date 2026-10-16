//! Email notifications reach an SMTP server (a minimal in-test sink).

use std::sync::{Arc, Mutex};

use flux_server::auth::{AuthMode, Principal};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn smtp_sink() -> (String, Arc<Mutex<Vec<String>>>) {
    let got: Arc<Mutex<Vec<String>>> = Default::default();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let store = got.clone();
    tokio::spawn(async move {
        loop {
            let (sock, _) = l.accept().await.unwrap();
            let store = store.clone();
            tokio::spawn(async move {
                let (r, mut w) = sock.into_split();
                let mut r = BufReader::new(r);
                w.write_all(b"220 sink\r\n").await.unwrap();
                let mut data = false;
                let mut msg = String::new();
                let mut line = String::new();
                while r.read_line(&mut line).await.unwrap_or(0) > 0 {
                    if data {
                        if line == ".\r\n" {
                            data = false;
                            store.lock().unwrap().push(std::mem::take(&mut msg));
                            w.write_all(b"250 queued\r\n").await.unwrap();
                        } else {
                            msg.push_str(&line);
                        }
                    } else {
                        let cmd = line.to_ascii_uppercase();
                        let reply: &[u8] = if cmd.starts_with("EHLO") {
                            b"250-sink\r\n250 OK\r\n"
                        } else if cmd.starts_with("DATA") {
                            data = true;
                            b"354 go\r\n"
                        } else if cmd.starts_with("QUIT") {
                            let _ = w.write_all(b"221 bye\r\n").await;
                            return;
                        } else {
                            b"250 OK\r\n"
                        };
                        w.write_all(reply).await.unwrap();
                    }
                    line.clear();
                }
            });
        }
    });
    (format!("smtp://{addr}"), got)
}

#[tokio::test]
async fn mentions_are_emailed() {
    let (url, got) = smtp_sink().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = flux_server::StorageConfig::data_dir(dir.path()).unwrap();
    let settings = flux_server::settings::Settings {
        smtp: Some(flux_server::settings::SmtpConfig { url, from: "flux <flux@example.com>".into() }),
        public_url: Some("https://flux.example.com".into()),
        ..Default::default()
    };
    let flux = flux_server::open_with_settings(&cfg, AuthMode::Token, flux_server::identity::SignupPolicy::Open, settings)
        .await
        .unwrap();
    flux.start_background();
    let alice = flux.sign_up("alice", Some("alice@example.com"), "Alice", "correct horse battery").await.unwrap();
    let bob = flux.sign_up("bob", Some("bob@example.com"), "Bob", "correct horse battery").await.unwrap();
    let repo = flux.create_repo("app").await.unwrap();
    let who = |u: &flux_store::User| Principal { user: Some(u.clone()), mode: AuthMode::Token, ..Default::default() };
    let issue = flux
        .open_issue(
            &who(&alice),
            repo,
            flux_server::issues::IssueInput { title: "Crash".into(), body: "@bob please look".into(), ..Default::default() },
        )
        .await
        .unwrap();
    let _ = bob;
    for _ in 0..100 {
        if !got.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let mails = got.lock().unwrap().clone();
    assert_eq!(mails.len(), 1, "only bob (mentioned) is emailed, not the author");
    let m = &mails[0];
    assert!(m.contains("To: Bob <bob@example.com>"), "{m}");
    assert!(m.contains(&format!("Subject: [app] Crash (#{})", issue.number)), "{m}");
    assert!(m.contains("https://flux.example.com/app/-/issues/"), "{m}");
}
