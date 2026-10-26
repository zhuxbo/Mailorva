use super::*;
use crate::{config::Config, mail::*};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use webhook::Reply;

const SECRET: &str = "whsec_YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE=";
const REPLACEMENT: &str = "whsec_YmJiYmJiYmJiYmJiYmJiYmJiYmJiYmJiYmJiYmJiYmI=";
#[derive(Default)]
struct Receiver {
    fail_challenge: AtomicBool,
    status: AtomicU16,
    verifications: AtomicUsize,
    events: std::sync::Mutex<Vec<(String, Value, bool)>>,
    arrived: Notify,
}
#[async_trait::async_trait]
impl Webhook for Receiver {
    async fn send(&self, delivery: Delivery<'_>) -> Result<Reply, CallbackError> {
        let value: Value = serde_json::from_str(delivery.body).unwrap();
        if value["type"] == "verification" {
            self.verifications.fetch_add(1, Ordering::SeqCst);
            let challenge = if self.fail_challenge.load(Ordering::SeqCst) {
                json!("wrong")
            } else {
                value["challenge"].clone()
            };
            return Ok(Reply {
                status: 200,
                body: json!({"challenge":challenge}).to_string().into_bytes(),
            });
        }
        self.events.lock().unwrap().push((
            delivery.id.into(),
            value,
            delivery.previous_secret.is_some(),
        ));
        self.arrived.notify_one();
        Ok(Reply {
            status: self.status.load(Ordering::SeqCst).max(200),
            body: vec![],
        })
    }
}
struct Mail;
#[async_trait::async_trait]
impl MailTransport for Mail {
    async fn snapshot(&self, _: &MailAccount, _: &str, _: i64) -> Result<MailboxState, MailError> {
        Ok(MailboxState {
            uid_validity: 1,
            uid_next: 2,
            uids: vec![1],
        })
    }
    async fn fetch(
        &self,
        _: &MailAccount,
        _: &str,
        _: u32,
        uid: u32,
    ) -> Result<MailMessage, MailError> {
        Ok(mail(uid, now()))
    }
    async fn set_flags(
        &self,
        _: &MailAccount,
        _: &str,
        _: u32,
        _: u32,
        _: Option<bool>,
        _: Option<bool>,
    ) -> Result<(), MailError> {
        panic!("unexpected mailbox write")
    }
    async fn move_message(
        &self,
        _: &MailAccount,
        _: &str,
        _: u32,
        _: u32,
        _: &str,
    ) -> Result<(), MailError> {
        Ok(())
    }
    async fn attachment(
        &self,
        _: &MailAccount,
        _: &str,
        _: u32,
        _: u32,
        _: usize,
    ) -> Result<AttachmentData, MailError> {
        panic!("unexpected attachment read")
    }
    async fn send(&self, _: &MailAccount, _: &OutgoingMail) -> Result<SendResult, MailError> {
        panic!("unexpected SMTP send")
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    s: Arc<Service>,
    receiver: Arc<Receiver>,
}
impl Fixture {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("read"), "r".repeat(32)).unwrap();
        std::fs::write(temp.path().join("write"), "w".repeat(32)).unwrap();
        std::fs::write(temp.path().join("password"), "test-password").unwrap();
        let account = |id: &str| {
            json!({"id":id,"email":format!("{id}@example.com"),"display_name":id,
            "imap_host":"imap.example.com","smtp_host":"smtp.example.com","password":{"file":temp.path().join("password")},"folders":["INBOX","Archive"]})
        };
        let config: Config = serde_json::from_value(json!({"database_path":temp.path().join("mail.sqlite"),
            "read_token":{"file":temp.path().join("read")},"write_token":{"file":temp.path().join("write")},"accounts":[account("a"),account("b")]})).unwrap();
        let mut s = Service::new(config, Arc::new(Mail)).await.unwrap();
        let receiver = Arc::new(Receiver::default());
        Arc::get_mut(&mut s).unwrap().events = Arc::new(Events {
            wake: Notify::new(),
            gate: Mutex::new(HashMap::new()),
            webhook: receiver.clone(),
        });
        Self {
            _temp: temp,
            s,
            receiver,
        }
    }
    fn access(&self) -> Access {
        Access {
            write: false,
            oauth: false,
            grant: None,
            event_owner: format!("token:{}", digest(&"r".repeat(32))),
        }
    }
    fn input(&self, filters: Value) -> Value {
        json!({"name":EVENT,"arguments":filters,"delivery":{"mode":"webhook","url":"https://receiver.example/callback","secret":SECRET}})
    }
    async fn subscribe(&self, filters: Value) -> Value {
        self.s
            .events
            .request(
                &self.s,
                &self.access(),
                "events/subscribe",
                self.input(filters),
            )
            .await
            .unwrap()
    }
    async fn put(&self, account: &str, folder: &str, uid: u32, date: i64) {
        self.s
            .db
            .put(
                account,
                &format!("{account}@example.com"),
                folder,
                1,
                mail(uid, date),
            )
            .await
            .unwrap();
    }
    async fn pending(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM event_deliveries")
            .fetch_one(&self.s.db.pool)
            .await
            .unwrap()
    }
}
fn mail(uid: u32, received_at: i64) -> MailMessage {
    MailMessage {
        uid,
        received_at,
        subject: format!("新邮件 {uid}"),
        from_address: "sender@example.com".into(),
        to_addresses: vec![],
        cc_addresses: vec![],
        reply_to_addresses: vec![],
        delivery_addresses: vec![],
        body_text: "不能放进事件的正文".into(),
        message_id: None,
        references: vec![],
        flags: vec![],
        attachments: vec![],
    }
}

#[tokio::test]
async fn subscription_verification_idempotency_rotation_and_ownership() {
    let f = Fixture::new().await;
    let first = f
        .subscribe(json!({"account_id":"a","folder":"INBOX"}))
        .await;
    let second = f
        .subscribe(json!({"folder":"INBOX","account_id":"a"}))
        .await;
    assert_eq!(first["id"], second["id"]);
    assert_eq!(f.receiver.verifications.load(Ordering::SeqCst), 1);
    let mut input = f.input(json!({"account_id":"a","folder":"INBOX"}));
    input["delivery"]["secret"] = json!(REPLACEMENT);
    let rotated =
        f.s.events
            .request(&f.s, &f.access(), "events/subscribe", input.clone())
            .await
            .unwrap();
    assert_eq!(first["id"], rotated["id"]);
    f.put("a", "INBOX", 1, now()).await;
    f.s.events.deliver_pending(&f.s).await.unwrap();
    assert!(f.receiver.events.lock().unwrap()[0].2);
    let write = Access {
        write: true,
        oauth: false,
        grant: None,
        event_owner: format!("token:{}", digest(&"w".repeat(32))),
    };
    f.s.events
        .request(&f.s, &write, "events/unsubscribe", input.clone())
        .await
        .unwrap();
    f.put("a", "INBOX", 2, now()).await;
    assert_eq!(
        f.pending().await,
        1,
        "another principal must not cancel the subscription"
    );
    f.s.events
        .request(&f.s, &f.access(), "events/unsubscribe", input.clone())
        .await
        .unwrap();
    f.s.events
        .request(&f.s, &f.access(), "events/unsubscribe", input)
        .await
        .unwrap();
    assert_eq!(f.pending().await, 0);
}

#[tokio::test]
async fn failed_verification_never_creates_a_subscription() {
    let f = Fixture::new().await;
    f.receiver.fail_challenge.store(true, Ordering::SeqCst);
    let error =
        f.s.events
            .request(&f.s, &f.access(), "events/subscribe", f.input(json!({})))
            .await
            .unwrap_err();
    assert_eq!(error.code, ErrorCode(-32015));
    f.put("a", "INBOX", 1, now()).await;
    assert_eq!(f.pending().await, 0);
}

#[tokio::test]
async fn enqueue_is_atomic_filtered_and_does_not_replay_history_or_updates() {
    let f = Fixture::new().await;
    f.put("a", "INBOX", 1, now()).await;
    f.subscribe(json!({"account_id":"a","folder":"INBOX"}))
        .await;
    f.put("a", "INBOX", 1, now()).await;
    f.put("a", "INBOX", 2, now() - 3600).await;
    f.put("b", "INBOX", 3, now()).await;
    f.put("a", "Archive", 4, now()).await;
    f.put("a", "INBOX", 5, now()).await;
    assert_eq!(f.pending().await, 1);
    sqlx::raw_sql("CREATE TRIGGER fail_outbox BEFORE INSERT ON event_deliveries BEGIN SELECT RAISE(ABORT,'test failure'); END;").execute(&f.s.db.pool).await.unwrap();
    assert!(f
        .s
        .db
        .put("a", "a@example.com", "INBOX", 1, mail(6, now()))
        .await
        .is_err());
    assert!(matches!(
        f.s.db.message("a:494e424f58:1:6").await,
        Err(crate::error::AppError::NotFound)
    ));
    assert_eq!(f.pending().await, 1);
    f.s.events.deliver_pending(&f.s).await.unwrap();
    let delivered = f.receiver.events.lock().unwrap();
    assert_eq!(delivered[0].1["data"]["message_id"], "a:494e424f58:1:5");
    assert!(!delivered[0].1.to_string().contains("不能放进事件的正文"));
}

#[tokio::test]
async fn transient_delivery_retries_same_event_after_database_reopen() {
    let f = Fixture::new().await;
    f.subscribe(json!({})).await;
    f.put("a", "INBOX", 1, now()).await;
    f.receiver.status.store(503, Ordering::SeqCst);
    f.s.events.deliver_pending(&f.s).await.unwrap();
    assert_eq!(f.pending().await, 1);
    f.s.events.deliver_pending(&f.s).await.unwrap();
    assert_eq!(
        f.receiver.events.lock().unwrap().len(),
        1,
        "retry must respect backoff"
    );
    let mut restarted = Service::new(f.s.config.clone(), Arc::new(Mail))
        .await
        .unwrap();
    Arc::get_mut(&mut restarted).unwrap().events = Arc::new(Events {
        wake: Notify::new(),
        gate: Mutex::new(HashMap::new()),
        webhook: f.receiver.clone(),
    });
    sqlx::query("UPDATE event_deliveries SET next_at=0")
        .execute(&restarted.db.pool)
        .await
        .unwrap();
    f.receiver.status.store(204, Ordering::SeqCst);
    restarted.events.deliver_pending(&restarted).await.unwrap();
    assert_eq!(f.pending().await, 0);
    let events = f.receiver.events.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, events[1].0);
    assert_eq!(events[0].1, events[1].1);
}

#[tokio::test]
async fn expiry_account_removal_and_token_rotation_stop_pending_delivery() {
    for scenario in ["expired", "removed", "rotated"] {
        let f = Fixture::new().await;
        f.subscribe(json!({})).await;
        f.put("a", "INBOX", 1, now()).await;
        match scenario {
            "expired" => {
                sqlx::query("UPDATE event_subscriptions SET expires_at=0")
                    .execute(&f.s.db.pool)
                    .await
                    .unwrap();
            }
            "removed" => {
                f.s.delete_account("a").await.unwrap();
            }
            _ => {
                std::fs::write(f._temp.path().join("read"), "x".repeat(32)).unwrap();
            }
        }
        f.s.events.deliver_pending(&f.s).await.unwrap();
        assert!(f.receiver.events.lock().unwrap().is_empty(), "{scenario}");
        assert_eq!(f.pending().await, 0, "{scenario}");
    }
}

#[tokio::test]
async fn terminal_http_errors_and_exhausted_retries_do_not_loop() {
    for status in [400, 410, 413, 503] {
        let f = Fixture::new().await;
        f.subscribe(json!({})).await;
        f.put("a", "INBOX", 1, now()).await;
        f.receiver.status.store(status, Ordering::SeqCst);
        for _ in 0..8 {
            sqlx::query("UPDATE event_deliveries SET next_at=0")
                .execute(&f.s.db.pool)
                .await
                .unwrap();
            f.s.events.deliver_pending(&f.s).await.unwrap();
        }
        assert_eq!(f.pending().await, 0);
        let subscriptions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM event_subscriptions")
            .fetch_one(&f.s.db.pool)
            .await
            .unwrap();
        assert_eq!(subscriptions, if status == 410 { 0 } else { 1 });
        assert_eq!(
            f.receiver.events.lock().unwrap().len(),
            if status == 503 { 6 } else { 1 }
        );
    }
}

#[tokio::test]
async fn permanent_subscription_defaults_null_and_survives_restart() {
    for explicit_null in [false, true] {
        let f = Fixture::new().await;
        let mut input = f.input(json!({}));
        if explicit_null {
            input["ttlMs"] = Value::Null;
        }
        let response =
            f.s.events
                .request(&f.s, &f.access(), "events/subscribe", input)
                .await
                .unwrap();
        assert_eq!(response.get("refreshBefore"), Some(&Value::Null));
        let expires: i64 = sqlx::query_scalar("SELECT expires_at FROM event_subscriptions")
            .fetch_one(&f.s.db.pool)
            .await
            .unwrap();
        assert_eq!(expires, i64::MAX);
        let mut restarted = Service::new(f.s.config.clone(), Arc::new(Mail))
            .await
            .unwrap();
        Arc::get_mut(&mut restarted).unwrap().events = Arc::new(Events {
            wake: Notify::new(),
            gate: Mutex::new(HashMap::new()),
            webhook: f.receiver.clone(),
        });
        f.put("a", "INBOX", 1, now() + 2 * 86400).await;
        restarted.events.deliver_pending(&restarted).await.unwrap();
        assert_eq!(f.receiver.events.lock().unwrap().len(), 1);
        assert_eq!(f.pending().await, 0);
    }
}

#[tokio::test]
async fn finite_subscription_can_be_converted_to_permanent_without_replaying() {
    let f = Fixture::new().await;
    let mut input = f.input(json!({}));
    input["ttlMs"] = json!(60_000);
    let first =
        f.s.events
            .request(&f.s, &f.access(), "events/subscribe", input.clone())
            .await
            .unwrap();
    assert!(first["refreshBefore"].is_string());
    let original: i64 = sqlx::query_scalar("SELECT created_at FROM event_subscriptions")
        .fetch_one(&f.s.db.pool)
        .await
        .unwrap();
    f.put("a", "INBOX", 1, now()).await;
    input["ttlMs"] = Value::Null;
    let second =
        f.s.events
            .request(&f.s, &f.access(), "events/subscribe", input.clone())
            .await
            .unwrap();
    assert_eq!(first["id"], second["id"]);
    assert_eq!(second.get("refreshBefore"), Some(&Value::Null));
    let created: i64 = sqlx::query_scalar("SELECT created_at FROM event_subscriptions")
        .fetch_one(&f.s.db.pool)
        .await
        .unwrap();
    assert_eq!(created, original);
    assert_eq!(f.pending().await, 1);
    f.s.events
        .request(&f.s, &f.access(), "events/unsubscribe", input)
        .await
        .unwrap();
    assert_eq!(f.pending().await, 0);
}

#[tokio::test]
async fn finite_ttl_still_validates_and_caps_requested_duration() {
    let f = Fixture::new().await;
    for ttl in [json!(0), json!(999), json!(-1), json!("forever")] {
        let mut input = f.input(json!({}));
        input["ttlMs"] = ttl;
        assert!(f
            .s
            .events
            .request(&f.s, &f.access(), "events/subscribe", input)
            .await
            .is_err());
    }
    for ttl in [1000_u64, 60_000, u64::MAX] {
        let start = now();
        let mut input = f.input(json!({}));
        input["ttlMs"] = json!(ttl);
        let response =
            f.s.events
                .request(&f.s, &f.access(), "events/subscribe", input)
                .await
                .unwrap();
        let expires: i64 = sqlx::query_scalar("SELECT expires_at FROM event_subscriptions")
            .fetch_one(&f.s.db.pool)
            .await
            .unwrap();
        let granted = (ttl.min(86_400_000) / 1000) as i64;
        assert!((start + granted..=now() + granted).contains(&expires));
        assert_eq!(response["refreshBefore"], timestamp(expires));
    }
}

#[tokio::test]
async fn inactive_owners_are_cleaned_even_without_pending_mail() {
    let f = Fixture::new().await;
    f.subscribe(json!({})).await;
    std::fs::write(f._temp.path().join("read"), "x".repeat(32)).unwrap();
    f.s.events.deliver_pending(&f.s).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM event_subscriptions")
        .fetch_one(&f.s.db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(f.receiver.events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn removed_filter_is_cleaned_without_deleting_unfiltered_subscription() {
    let f = Fixture::new().await;
    f.subscribe(json!({"account_id":"a","folder":"INBOX"}))
        .await;
    let unfiltered = f.subscribe(json!({})).await;
    f.s.delete_account("a").await.unwrap();
    f.s.events.deliver_pending(&f.s).await.unwrap();
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM event_subscriptions")
        .fetch_all(&f.s.db.pool)
        .await
        .unwrap();
    assert_eq!(ids, vec![unfiltered["id"].as_str().unwrap()]);
}

#[tokio::test]
async fn sync_wakes_delivery_without_waiting_for_the_next_sync_cycle() {
    let f = Fixture::new().await;
    f.subscribe(json!({"account_id":"a","folder":"INBOX"}))
        .await;
    f.s.db.set_cursor("a", "INBOX", 1, 0).await.unwrap();
    let cancel = CancellationToken::new();
    let worker = tokio::spawn(f.s.events.clone().run(f.s.clone(), cancel.clone()));
    // Wait for the startup scan to deliver an existing event, then exercise the sync wakeup.
    // The worker's retry timer cannot account for a delivery in the next second.
    f.put("a", "INBOX", 99, now()).await;
    f.s.events.wake.notify_one();
    tokio::time::timeout(Duration::from_secs(1), f.receiver.arrived.notified())
        .await
        .unwrap();
    f.s.sync_account("a").await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), f.receiver.arrived.notified())
        .await
        .unwrap();
    cancel.cancel();
    worker.await.unwrap();
    assert_eq!(f.receiver.events.lock().unwrap().len(), 2);
    assert_eq!(f.s.config.sync_interval_seconds, 60);
}

#[tokio::test]
async fn changed_token_file_does_not_rebind_an_authenticated_request_to_new_credentials() {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let f = Fixture::new().await;
    let app = crate::api::router(f.s.clone()).unwrap();
    std::fs::write(f._temp.path().join("read"), "x".repeat(32)).unwrap();
    let body =
        json!({"jsonrpc":"2.0","id":1,"method":"events/subscribe","params":f.input(json!({}))});
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("host", "localhost")
                .header("authorization", format!("Bearer {}", "r".repeat(32)))
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        value["error"].is_object(),
        "old token must not create a subscription owned by the new token: {value}"
    );
}

#[tokio::test]
async fn refreshing_from_verification_cache_does_not_extend_the_cache_forever() {
    let f = Fixture::new().await;
    f.subscribe(json!({})).await;
    let verified_at = now() - 100;
    sqlx::query("UPDATE event_subscriptions SET verified_at=?")
        .bind(verified_at)
        .execute(&f.s.db.pool)
        .await
        .unwrap();
    f.subscribe(json!({})).await;
    let actual: i64 = sqlx::query_scalar("SELECT verified_at FROM event_subscriptions")
        .fetch_one(&f.s.db.pool)
        .await
        .unwrap();
    assert_eq!(
        actual, verified_at,
        "only a successful challenge can advance verification time"
    );
    sqlx::query("UPDATE event_subscriptions SET verified_at=?")
        .bind(now() - 301)
        .execute(&f.s.db.pool)
        .await
        .unwrap();
    f.subscribe(json!({})).await;
    assert_eq!(f.receiver.verifications.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn moved_mail_keeps_pending_event_and_does_not_notify_twice_after_restart() {
    for has_message_id in [true, false] {
        let f = Fixture::new().await;
        f.subscribe(json!({"account_id":"a"})).await;
        let date = now();
        let mut original = mail(7, date);
        original.message_id = has_message_id.then(|| "<move@example.com>".into());
        f.s.db
            .put("a", "a@example.com", "INBOX", 1, original)
            .await
            .unwrap();
        let id: String = sqlx::query_scalar("SELECT event_id FROM event_deliveries")
            .fetch_one(&f.s.db.pool)
            .await
            .unwrap();
        f.s.move_message("a:494e424f58:1:7", "Archive")
            .await
            .unwrap();
        assert_eq!(f.pending().await, 1, "moving must not delete the outbox");
        f.s.events.deliver_pending(&f.s).await.unwrap();
        assert_eq!(f.receiver.events.lock().unwrap()[0].0, id);
        let restarted = Service::new(f.s.config.clone(), Arc::new(Mail))
            .await
            .unwrap();
        let mut original = mail(7, date);
        original.message_id = has_message_id.then(|| "<move@example.com>".into());
        original.uid = 42;
        restarted
            .db
            .put("a", "a@example.com", "Archive", 9, original)
            .await
            .unwrap();
        assert_eq!(f.pending().await, 0, "moving must not create another event");
    }
}

struct SlowReceiver {
    entered: Notify,
    release: Notify,
    healthy: Notify,
}
#[async_trait::async_trait]
impl Webhook for SlowReceiver {
    async fn send(&self, delivery: Delivery<'_>) -> Result<Reply, CallbackError> {
        let value: Value = serde_json::from_str(delivery.body).unwrap();
        if delivery.verification {
            return Ok(Reply {
                status: 200,
                body: json!({"challenge":value["challenge"]})
                    .to_string()
                    .into_bytes(),
            });
        }
        if value["data"]["account_id"] == "a" {
            self.entered.notify_one();
            self.release.notified().await;
        } else {
            self.healthy.notify_one();
        }
        Ok(Reply {
            status: 204,
            body: vec![],
        })
    }
}

#[tokio::test]
async fn slow_callback_does_not_block_other_deliveries_or_unsubscribe() {
    let mut f = Fixture::new().await;
    let receiver = Arc::new(SlowReceiver {
        entered: Notify::new(),
        release: Notify::new(),
        healthy: Notify::new(),
    });
    Arc::get_mut(&mut f.s).unwrap().events = Arc::new(Events {
        webhook: receiver.clone(),
        ..Events::default()
    });
    f.subscribe(json!({"account_id":"a"})).await;
    f.subscribe(json!({"account_id":"b"})).await;
    f.put("a", "INBOX", 1, now()).await;
    f.put("b", "INBOX", 2, now()).await;
    let s = f.s.clone();
    let task = tokio::spawn(async move { s.events.deliver_pending(&s).await });
    tokio::time::timeout(Duration::from_secs(1), receiver.entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), receiver.healthy.notified())
        .await
        .expect("healthy subscription must proceed while another callback is slow");
    tokio::time::timeout(
        Duration::from_secs(1),
        f.s.events.request(
            &f.s,
            &f.access(),
            "events/unsubscribe",
            f.input(json!({"account_id":"a"})),
        ),
    )
    .await
    .expect("unsubscribe must not wait for callback")
    .unwrap();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("cancellation must stop the local in-flight request")
        .unwrap()
        .unwrap();
    assert_eq!(f.pending().await, 0);
}

#[tokio::test]
async fn fingerprints_distinguish_accounts_and_reused_message_ids_and_expire() {
    let f = Fixture::new().await;
    f.subscribe(json!({})).await;
    let date = now();
    for (account, folder, uid, body) in [
        ("a", "INBOX", 1, "original"),
        ("a", "Archive", 2, "original"),
        ("a", "Archive", 3, "different"),
        ("b", "INBOX", 1, "original"),
    ] {
        let mut m = mail(1, date);
        m.uid = uid;
        m.message_id = Some("<reused@example.com>".into());
        m.body_text = body.into();
        f.s.db
            .put(account, "test@example.com", folder, 1, m)
            .await
            .unwrap();
    }
    assert_eq!(f.pending().await, 3);
    f.s.db
        .cleanup(date + crate::model::RETENTION_SECONDS + 1)
        .await
        .unwrap();
    assert_eq!(f.pending().await, 0);
    let seen: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM event_seen")
        .fetch_one(&f.s.db.pool)
        .await
        .unwrap();
    assert_eq!(seen, 0);
}

#[tokio::test]
async fn running_worker_delivers_new_events_while_another_callback_is_waiting() {
    let mut f = Fixture::new().await;
    let receiver = Arc::new(SlowReceiver {
        entered: Notify::new(),
        release: Notify::new(),
        healthy: Notify::new(),
    });
    Arc::get_mut(&mut f.s).unwrap().events = Arc::new(Events {
        webhook: receiver.clone(),
        ..Events::default()
    });
    f.subscribe(json!({"account_id":"a"})).await;
    f.subscribe(json!({"account_id":"b"})).await;
    f.put("a", "INBOX", 1, now()).await;
    let cancel = CancellationToken::new();
    let worker = tokio::spawn(f.s.events.clone().run(f.s.clone(), cancel.clone()));
    tokio::time::timeout(Duration::from_secs(1), receiver.entered.notified())
        .await
        .unwrap();
    for uid in [2, 3] {
        f.put("b", "INBOX", uid, now()).await;
        f.s.events.wake.notify_one();
        tokio::time::timeout(Duration::from_secs(1), receiver.healthy.notified())
            .await
            .expect("new events must not wait for slow callbacks in earlier scans");
    }
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(1), worker)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn renewing_expired_subscription_cancels_its_old_callback() {
    let mut f = Fixture::new().await;
    let receiver = Arc::new(SlowReceiver {
        entered: Notify::new(),
        release: Notify::new(),
        healthy: Notify::new(),
    });
    Arc::get_mut(&mut f.s).unwrap().events = Arc::new(Events {
        webhook: receiver.clone(),
        ..Events::default()
    });
    f.subscribe(json!({"account_id":"a"})).await;
    f.put("a", "INBOX", 1, now()).await;
    let s = f.s.clone();
    let delivery = tokio::spawn(async move { s.events.deliver_pending(&s).await });
    tokio::time::timeout(Duration::from_secs(1), receiver.entered.notified())
        .await
        .unwrap();
    sqlx::query("UPDATE event_subscriptions SET expires_at=0")
        .execute(&f.s.db.pool)
        .await
        .unwrap();
    f.subscribe(json!({"account_id":"a"})).await;
    tokio::time::timeout(Duration::from_secs(1), delivery)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(f.pending().await, 0);
    f.put("a", "INBOX", 2, now()).await;
    let s = f.s.clone();
    let delivery = tokio::spawn(async move { s.events.deliver_pending(&s).await });
    tokio::time::timeout(Duration::from_secs(1), receiver.entered.notified())
        .await
        .unwrap();
    receiver.release.notify_one();
    delivery.await.unwrap().unwrap();
    assert_eq!(f.pending().await, 0);
}
