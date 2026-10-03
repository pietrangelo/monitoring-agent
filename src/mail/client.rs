// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Pietrangelo Masala
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! The mail client (RFC 0017 §5), the adapter: every 2 s it offers the published snapshot to
//! the batch and notes the active alerts; at each mail interval, or at once for a new
//! incident (at most one a minute), it closes the batch, seals and armours the report, and
//! queues the message; the outbox sends it through the relay with lettre's Tokio transport,
//! so nothing blocks the runtime.

use std::sync::Arc;
use std::time::{Duration, Instant};

use lettre::message::Mailbox;
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Certificate, Tls as SmtpTls, TlsParameters};
use lettre::{Address, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use super::batch::{IncidentPace, MailBatch};
use super::config::{MailSettings, MailTls};
use super::outbox::{Outbox, SendResult};
use super::report::{self, MailReport, MailedSnapshot, ReportReason};
use super::seal::{self, Nonce};
use crate::push::identity::AgentId;
use crate::state::AppState;

/// How often the client looks at the snapshot and the alerts: the alert tick.
const TICK: Duration = Duration::from_secs(2);
/// How long one SMTP session may take.
const SMTP_TIMEOUT: Duration = Duration::from_secs(30);

type Transport = AsyncSmtpTransport<Tokio1Executor>;

/// What the client needs, resolved at startup.
pub struct MailTarget {
    pub settings: MailSettings,
    pub system_id: AgentId,
    /// `MAIL_RELAY_CA`'s PEM, read before the runtime.
    pub relay_ca: Option<Vec<u8>>,
    /// `MAIL_FROM`, or `system-agent@<host name>`.
    pub from: Address,
}

/// The SMTP transport the settings describe. Certificate verification is never off.
pub fn transport(
    settings: &MailSettings,
    relay_ca: Option<&[u8]>,
) -> Result<Transport, lettre::transport::smtp::Error> {
    let host = settings.relay.host.clone();
    let params = || {
        let mut builder = TlsParameters::builder(host.clone());
        if let Some(pem) = relay_ca {
            builder = builder.add_root_certificate(Certificate::from_pem(pem)?);
        }
        builder.build()
    };
    let tls = match settings.tls {
        MailTls::StartTls => SmtpTls::Required(params()?),
        MailTls::Tls => SmtpTls::Wrapper(params()?),
        MailTls::None => SmtpTls::None,
    };
    let mut builder = Transport::builder_dangerous(host)
        .port(settings.relay.port)
        .tls(tls)
        .timeout(Some(SMTP_TIMEOUT));
    if let Some(credentials) = &settings.credentials {
        builder = builder.credentials(Credentials::new(
            credentials.username.clone(),
            credentials.password.clone(),
        ));
    }
    Ok(builder.build())
}

/// The message carrying one armoured report. The builder encodes every header, so no value
/// can inject one.
pub fn message(
    from: &Address,
    to: &Address,
    system_id: &AgentId,
    report: &MailReport,
    armoured: String,
) -> Result<Message, lettre::error::Error> {
    let subject = format!(
        "system-agent report {} {}-{}",
        system_id.as_str(),
        report.id.run.as_uuid().hyphenated(),
        report.id.seq
    );
    Message::builder()
        .from(Mailbox::new(None, from.clone()))
        .to(Mailbox::new(None, to.clone()))
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(armoured)
}

/// Sends one message: a 5xx is permanent, anything else that fails is temporary.
pub async fn send(transport: &Transport, message: Message) -> SendResult {
    match transport.send(message).await {
        Ok(_) => SendResult::Sent,
        Err(err) if err.is_permanent() => {
            tracing::error!("The mail relay refused a report for good; dropping it: {err}");
            SendResult::Permanent
        }
        Err(err) => {
            tracing::warn!("The mail relay didn't take a report; retrying later: {err}");
            SendResult::Temporary
        }
    }
}

/// Seals and armours a report into its message, with a nonce from the OS RNG.
fn sealed_message(target: &MailTarget, report: &MailReport) -> Option<Message> {
    let plaintext = match report::encode(report) {
        Ok(plaintext) => plaintext,
        Err(err) => {
            tracing::warn!("Skipping a mail report that failed to encode: {err}");
            return None;
        }
    };
    let nonce = random_nonce();
    let sealed = seal::seal(&target.system_id, &plaintext, &target.settings.key, &nonce);
    let to = &target.settings.to;
    match message(
        &target.from,
        to,
        &target.system_id,
        report,
        seal::armour(&sealed),
    ) {
        Ok(message) => Some(message),
        Err(err) => {
            tracing::warn!("Skipping a mail report whose message failed to build: {err}");
            None
        }
    }
}

/// A nonce of 24 random bytes: two v4 UUIDs' 122 random bits each, from the OS RNG.
fn random_nonce() -> Nonce {
    let mut nonce = [0; 24];
    let (a, b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    nonce[..16].copy_from_slice(a.as_bytes());
    nonce[16..].copy_from_slice(&b.as_bytes()[..8]);
    Nonce(nonce)
}

/// Starts the mail client for the agent's lifetime.
pub fn spawn_mail_client(app: Arc<AppState>, target: MailTarget) {
    let transport = match transport(&target.settings, target.relay_ca.as_deref()) {
        Ok(transport) => transport,
        Err(err) => {
            tracing::error!("The mail transport couldn't be built: {err}; not mailing");
            return;
        }
    };
    tracing::info!(
        "📮 Mailing reports as {:?} every {} s",
        target.system_id.as_str(),
        target.settings.interval.as_duration().as_secs()
    );
    tokio::spawn(async move { run(app, target, transport).await });
}

/// What the client keeps between ticks.
struct Client {
    batch: MailBatch,
    pace: IncidentPace,
    outbox: Outbox<Message>,
    next_report: Instant,
    next_send: Instant,
    dropped_warned_at: Option<Instant>,
}

async fn run(app: Arc<AppState>, target: MailTarget, transport: Transport) {
    let interval = target.settings.interval.as_duration();
    let now = Instant::now();
    let mut client = Client {
        batch: MailBatch::new(app.run, target.settings.sample_interval),
        pace: IncidentPace::default(),
        outbox: Outbox::default(),
        next_report: now + interval,
        next_send: now,
        dropped_warned_at: None,
    };
    let mut ticks = tokio::time::interval(TICK);
    loop {
        ticks.tick().await;
        let now = Instant::now();
        let new_incident = gather(&app, &mut client.batch);
        if let Some(reason) = client.due(now, interval, new_incident) {
            client.close(&app, &target, reason);
        }
        if now >= client.next_send && client.outbox.front().is_some() {
            client.send_front(&transport, interval).await;
        }
        client.log_dropped(now);
    }
}

/// Offers the published snapshot, when fresh, and notes the active alerts.
fn gather(app: &AppState, batch: &mut MailBatch) -> bool {
    if let Ok(snapshot) = app.fresh_snapshot() {
        batch.offer(MailedSnapshot::from(snapshot.as_ref()), snapshot.read_at);
    }
    let alerts = app.alert_manager.read().active_alerts().to_vec();
    batch.note_alerts(&alerts)
}

impl Client {
    /// Whether a report is due at `now`: the interval has passed, or an incident became
    /// active and the incident pace allows one.
    fn due(
        &mut self,
        now: Instant,
        interval: Duration,
        new_incident: bool,
    ) -> Option<ReportReason> {
        if now >= self.next_report {
            self.next_report = now + interval;
            return Some(ReportReason::Scheduled);
        }
        (new_incident && self.pace.allows(now)).then_some(ReportReason::Incident)
    }

    /// Closes the batch into a report and queues its message.
    fn close(&mut self, app: &AppState, target: &MailTarget, reason: ReportReason) {
        let round = app
            .rounds
            .as_ref()
            .and_then(|rounds| rounds.borrow().clone());
        let round = round.map(|round| round.as_ref().clone());
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let interval = target.settings.interval;
        if let Some(report) = self.batch.close(interval, reason, created_at, round)
            && let Some(message) = sealed_message(target, &report)
        {
            self.outbox.push(message);
        }
    }

    async fn send_front(&mut self, transport: &Transport, interval: Duration) {
        let Some(message) = self.outbox.front().cloned() else {
            return;
        };
        let result = send(transport, message).await;
        self.next_send = Instant::now() + self.outbox.after(result, interval);
    }

    /// Logs reports the outbox dropped, at most hourly.
    fn log_dropped(&mut self, now: Instant) {
        let dropped = self.outbox.take_dropped();
        let quiet = self
            .dropped_warned_at
            .is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(3600));
        if dropped > 0 && !quiet {
            tracing::warn!("The mail outbox dropped {dropped} report(s)");
            self.dropped_warned_at = Some(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::AgentRun;
    use crate::mail::config::{MailConfig, Relay};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    /// A relay that speaks just enough SMTP for one message, answers its end of DATA with
    /// `final_reply`, and hands back the DATA it received.
    async fn relay(final_reply: &'static str) -> (u16, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (read, mut write) = socket.into_split();
            let mut lines = BufReader::new(read).lines();
            write.write_all(b"220 relay ESMTP\r\n").await.unwrap();
            let mut data = String::new();
            let mut in_data = false;
            while let Ok(Some(line)) = lines.next_line().await {
                if in_data {
                    if line == "." {
                        in_data = false;
                        write.write_all(final_reply.as_bytes()).await.unwrap();
                    } else {
                        data.push_str(&line);
                        data.push('\n');
                    }
                    continue;
                }
                let reply: &[u8] = match line.get(..4).map(str::to_ascii_uppercase).as_deref() {
                    Some("EHLO") => b"250 relay\r\n",
                    Some("DATA") => {
                        in_data = true;
                        b"354 go ahead\r\n"
                    }
                    Some("QUIT") => {
                        let _ = write.write_all(b"221 bye\r\n").await;
                        break;
                    }
                    _ => b"250 ok\r\n",
                };
                write.write_all(reply).await.unwrap();
            }
            data
        });
        (port, served)
    }

    fn settings(port: u16) -> MailSettings {
        let vars = [
            ("MAIL_TO", "hub@example.org".to_string()),
            ("MAIL_RELAY", format!("127.0.0.1:{port}")),
            ("MAIL_TLS", "none".to_string()),
            (
                "MAIL_KEY",
                "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=".to_string(),
            ),
        ];
        let parsed = MailConfig::parse(|key| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
                .ok_or(std::env::VarError::NotPresent)
        });
        match parsed {
            Ok(MailConfig::On(settings)) => *settings,
            Ok(MailConfig::Off) | Err(_) => panic!("test settings parse"),
        }
    }

    fn a_message() -> Message {
        let report = crate::mail::report::tests::golden_report();
        let id = AgentId::try_from("web-01").unwrap();
        let from = Address::new("system-agent", "web-01.example").unwrap();
        let to = Address::new("hub", "example.org").unwrap();
        let _ = AgentRun::new(uuid::Uuid::nil());
        message(&from, &to, &id, &report, seal::armour(b"sealed bytes")).unwrap()
    }

    /// RFC 0017 §4, §5: the relay gets the armoured text; its answer decides what the outbox
    /// does: sent, retried later, or dropped.
    #[tokio::test]
    async fn the_relays_answer_decides_sent_temporary_or_permanent() {
        let cases = [
            ("accepted", "250 queued\r\n", SendResult::Sent),
            ("a 4xx", "451 try later\r\n", SendResult::Temporary),
            ("a 5xx", "554 rejected\r\n", SendResult::Permanent),
        ];
        for (name, reply, expected) in cases {
            let (port, served) = relay(reply).await;
            let transport = transport(&settings(port), None).unwrap();

            let result = send(&transport, a_message()).await;

            assert_eq!(result, expected, "case {name}");
            let data = served.await.unwrap();
            assert!(
                data.contains(seal::ARMOUR_BEGIN),
                "case {name}: the armour: {data}"
            );
            assert!(
                data.contains("Subject: system-agent report web-01"),
                "case {name}"
            );
        }
    }

    /// RFC 0017 §5: no relay listening is a temporary failure, never a dropped report.
    #[tokio::test]
    async fn an_unreachable_relay_is_temporary() {
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let transport = transport(&settings(port), None).unwrap();

        assert_eq!(send(&transport, a_message()).await, SendResult::Temporary);
        let _ = Relay {
            host: String::new(),
            port,
        };
    }
}
