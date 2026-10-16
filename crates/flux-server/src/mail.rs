//! Email notifications over SMTP.
//!
//! Notifications are queued in memory and sent by a background worker, so
//! the reactor never waits on a mail server. Each email links to the item
//! and to notification settings. Only accounts with an email address get
//! mail; a failed send is logged and dropped (the in-app inbox still has
//! the notification).

use std::sync::Arc;

use lettre::message::{Mailbox, header::ContentType};
use lettre::transport::smtp::AsyncSmtpTransport;
use lettre::{AsyncTransport, Message, Tokio1Executor};

use crate::core::Flux;

/// One email to send.
#[derive(Debug, Clone)]
pub struct Mail {
    pub to_user: i64,
    pub subject: String,
    pub body: String,
    /// Groups related messages in mail clients (one thread per item).
    pub thread: String,
}

pub(crate) fn start(flux: &Arc<Flux>) -> Option<tokio::sync::mpsc::Sender<Mail>> {
    let smtp = flux.settings.smtp.clone()?;
    let transport = match AsyncSmtpTransport::<Tokio1Executor>::from_url(&smtp.url) {
        Ok(b) => b.timeout(Some(std::time::Duration::from_secs(20))).build(),
        Err(e) => {
            tracing::error!(error = %e, "email: invalid --smtp-url; notifications will not be emailed");
            return None;
        }
    };
    let from: Mailbox = match smtp.from.parse() {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, "email: invalid --smtp-from");
            return None;
        }
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Mail>(10_000);
    let flux = flux.clone();
    tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let Ok(Some(acct)) = flux.meta.get_account_by_id(m.to_user).await else { continue };
            let Some(addr) = acct.email.as_deref().and_then(|e| e.parse::<lettre::Address>().ok()) else { continue };
            let to = Mailbox::new(Some(acct.display_name.clone()).filter(|n| !n.is_empty()), addr);
            let host =
                flux.settings.base_url().split("://").nth(1).unwrap_or("flux").split('/').next().unwrap_or("flux").to_string();
            let msg_id = format!("<{}@{host}>", m.thread);
            let email = Message::builder()
                .from(from.clone())
                .to(to)
                .subject(&m.subject)
                .in_reply_to(msg_id.clone())
                .references(msg_id)
                .header(ContentType::TEXT_PLAIN)
                .body(m.body.clone());
            let email = match email {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "email: could not build message");
                    continue;
                }
            };
            if let Err(e) = transport.send(email).await {
                tracing::warn!(error = %e, user = m.to_user, "email: send failed");
            }
        }
    });
    Some(tx)
}

impl Flux {
    /// Queue an email (no-op without SMTP; drops when the queue is full).
    pub(crate) fn queue_mail(&self, m: Mail) {
        if let Some(tx) = self.mailer.get().and_then(Option::as_ref)
            && tx.try_send(m).is_err()
        {
            tracing::warn!("email: queue full, dropping a notification email");
        }
    }
}
