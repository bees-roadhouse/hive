// OIDC account linking: the join key, and the four ways it has to refuse.
//
// The email-linking takeover — hand the local account to whoever arrives with
// its address in an id_token — was fixed, and had no test. It had already come
// back once in recovered code, which is exactly the kind of defect that needs a
// test rather than a comment. `docs/SELF-HOST.md` states the rule: link on
// `(issuer, subject)`, never on email, not even `email_verified`, because that
// is an assertion by a provider we do not control.
//
// One test function on purpose: `oauth.rs` caches OIDC discovery in a process
// -wide `OnceLock`, so two tests in one binary standing up two fake providers
// would race for it. One binary, one provider, one sequence. (Same reason this
// is a file of its own rather than more cases in parity_smoke.rs.)

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::routing::{get as route_get, post as route_post};
use axum::{Json, Router};
use hive_shared::ActorKind;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use std::collections::HashMap;
use tower::ServiceExt;

use hive_api::store::Store;

// Test key material, shared with parity_smoke.rs's OIDC fixture.
const OIDC_TEST_PRIVATE_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvwIBADANBgkqhkiG9w0BAQEFAASCBKkwggSlAgEAAoIBAQClUE7f4AGyfdwv
ycn1RBfPrCm8fTM+5m5DCgXeTi3b+ne7RpvlxHMfHVyyNMOLuE0TvXtR7HdO5tiI
XE5OBsKVHWUIE8KGTK6cM1jrcXsTuLoZYRUicGoxOzTUAQh6Ys+/i87K+1IgdIAK
OynROZvGkHdwqKxMoMmKANda9J+kuAtUJTLFYDcq9XjwOaJP5Z1BhYfTJ4dmF/Mu
EFqQBqKntpUq9b3syOuGWKAYgtD+kcAe0XQ6InibQrqi0e+COiQfKiD3dKudPM2R
FbhOa7u9e1aA+9ne0AO2nhQBJ2C9HuJg8ZAX5xskoaQxMoNn9msm5yc8l/y4HACF
Zio4L6M7AgMBAAECggEAF3/G9oP9OcYyWoiwsLCxQdATTrvtYO+YlOcD1on+ctqz
0mdDGfJG+xFNb/eYJHBaZIf207pta0XdWeTlLKpBVrkK9473g+e6mnGiHjXPbQpB
SgJG4tJgBgeIhupurhcFuRDCoJABKKPm341xcFBkGGHI2LbhZzMj8v4TntZPKzbT
Wq+UBHDbLuuHzbanH+qPQSIEiuGcVDR7L9eGRkKfAcgWoEgHtaSdmdUbNrJKvsYm
fTXGbVfpWDYW6Jk87mse1zEv+RvUf8/n/wrGYatiXwtgJzdImvAHn/XSa4mTbixi
Taj+rWR1Pxy6xDsRsicmY1pBf38gMpf+JbZKwyV5AQKBgQDoPv/SzRSqMKMprINz
0ISWK36X3hNqSx4qSzgCKzF8x9J1iMCEykL0V/ym2IK7HOz3vzqXdDW5E/Ng0Pem
8UrC8F5GniKMr/WwDbE2dDAo/Jy5F5VIcQfjmqpwqUqpW7VChkBjvlipJkyQouV3
nib5VZcIwY70vf/5EWpPIsr6WwKBgQC2OMn1WTga/6IJGjR0cew1Clc93WMOi9b1
EH8kruSck02GLWbL8JyFBLxX5SqVk8v9Jt04XAJUBmQmTdYsqAQ6wfnugsPzvR9U
gFlBpdzusDlR05jqZtbk3Qni1rVIsxZLDQ3TQBRA3yvOK8N8M4dSwHpN6s4fj+xN
3SnmCS+QoQKBgQCFHQ7OCSOOBICQc0OIzvwfgmB1tSCVrOZmQWShwZYEuhdDrJUD
x1Ym7INwMeqESqj7uwxfIIlmQiwd0sgPVH+QSesPOLX+wx/jv4VR+7ha1acSY5T5
x2dJKi4EktOrTFgRABfJ06DHmp8Jy4QQUoJuKIN/zkkcuAYOANBY+U0zvwKBgQCD
cJk1EeMnjmeqGy3lJNvWMpxVcqDmODaY1QpxQnqC+rn75Dn3N5sfVBgrapF6DX8i
HuuJoMzJIUcSXij0U0mhvJP02HxSD4RO5rn7YZHo1lKyVGhEBGRT96EO8AMZ6pxV
DJiBXgJ9/LzTXbwHlf+x0EcodwuxtpYkYDi9xrh5oQKBgQDPbF4qmQtk1TVrGvWD
QhOhxtRTzj9dTbjN2bfuSrG57XUzb2qOTBAbh7Iyaw2/NCvhLyRHUf1mJFcLScjq
1qb7yle/Uh5YYvL9/LqITKyK39OKWlWRTT4quasSc7vJ4a95dO7QZu3HMOZN477y
TcOrPdBrkCcLEQuH2iTbia2C+g==
-----END PRIVATE KEY-----"#;

const OIDC_TEST_JWK_N: &str = "pVBO3-ABsn3cL8nJ9UQXz6wpvH0zPuZuQwoF3k4t2_p3u0ab5cRzHx1csjTDi7hNE717Uex3TubYiFxOTgbClR1lCBPChkyunDNY63F7E7i6GWEVInBqMTs01AEIemLPv4vOyvtSIHSACjsp0TmbxpB3cKisTKDJigDXWvSfpLgLVCUyxWA3KvV48DmiT-WdQYWH0yeHZhfzLhBakAaip7aVKvW97MjrhligGILQ_pHAHtF0OiJ4m0K6otHvgjokHyog93SrnTzNkRW4Tmu7vXtWgPvZ3tADtp4UASdgvR7iYPGQF-cbJKGkMTKDZ_ZrJucnPJf8uBwAhWYqOC-jOw";

/// A fake IdP whose claims are steered by the auth code: `nonce|sub|email|name`.
/// `!missing` drops the `sub` claim entirely, `!empty` sends an empty one —
/// both are id_tokens that identify nobody.
async fn fake_oidc_provider() -> String {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route(
            "/.well-known/openid-configuration",
            route_get(|State(issuer): State<String>| async move {
                Json(json!({
                    "issuer": issuer,
                    "authorization_endpoint": format!("{issuer}/authorize"),
                    "token_endpoint": format!("{issuer}/token"),
                    "jwks_uri": format!("{issuer}/jwks"),
                }))
            }),
        )
        .route(
            "/jwks",
            route_get(|| async {
                Json(json!({
                    "keys": [{
                        "kty": "RSA",
                        "kid": "hive-test",
                        "use": "sig",
                        "alg": "RS256",
                        "n": OIDC_TEST_JWK_N,
                        "e": "AQAB",
                    }]
                }))
            }),
        )
        .route(
            "/token",
            route_post(
                |State(issuer): State<String>,
                 axum::Form(form): axum::Form<HashMap<String, String>>| async move {
                    let code = form.get("code").map(String::as_str).unwrap_or("");
                    let parts: Vec<&str> = code.split('|').collect();
                    let sub = parts.get(1).copied().unwrap_or("subject-default");
                    let email = parts.get(2).copied().unwrap_or("default@example.com");
                    let name = parts.get(3).copied().unwrap_or("Test Person");
                    let mut claims = json!({
                        "iss": issuer,
                        "aud": "hive-client",
                        "exp": chrono::Utc::now().timestamp() + 600,
                        "nonce": parts[0],
                        "email": email,
                        "name": name,
                    });
                    match sub {
                        "!missing" => {}
                        "!empty" => claims["sub"] = json!(""),
                        s => claims["sub"] = json!(s),
                    }
                    let mut header = Header::new(Algorithm::RS256);
                    header.kid = Some("hive-test".to_string());
                    let id_token = jsonwebtoken::encode(
                        &header,
                        &claims,
                        &EncodingKey::from_rsa_pem(OIDC_TEST_PRIVATE_KEY.as_bytes()).unwrap(),
                    )
                    .unwrap();
                    Json(json!({
                        "id_token": id_token,
                        "access_token": "fake-access-token",
                        "token_type": "Bearer",
                    }))
                },
            ),
        )
        .with_state(issuer.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    issuer
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, String, HeaderMap) {
    let res = app.clone().oneshot(req).await.expect("request");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        headers,
    )
}

fn get(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut b = Request::get(path);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    b.body(Body::empty()).expect("request")
}

fn post(path: &str, body: Value) -> Request<Body> {
    Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

struct SignIn {
    status: StatusCode,
    body: String,
    session: Option<String>,
}

impl SignIn {
    fn refused(&self, needle: &str) {
        assert_eq!(self.status, StatusCode::BAD_REQUEST, "{}", self.body);
        assert!(self.session.is_none(), "a refusal must not set a session");
        assert!(
            self.body.contains(needle),
            "expected {needle:?} in: {}",
            self.body
        );
    }
}

/// Walk start → IdP → callback with the claims this `sub`/`email` describe.
async fn sign_in(app: &Router, sub: &str, email: &str) -> SignIn {
    sign_in_as(app, sub, email, "Test Person").await
}

async fn sign_in_as(app: &Router, sub: &str, email: &str, name: &str) -> SignIn {
    let (status, _, headers) = send(app, get("/api/auth/oidc/start", None)).await;
    assert_eq!(status, StatusCode::FOUND, "oidc start");
    let location = headers
        .get(header::LOCATION)
        .expect("redirect")
        .to_str()
        .unwrap();
    let url = reqwest::Url::parse(location).unwrap();
    let param = |k: &str| {
        url.query_pairs()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.into_owned())
            .unwrap_or_else(|| panic!("{k} param"))
    };
    let (state, nonce) = (param("state"), param("nonce"));
    let cookies = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .collect::<Vec<_>>()
        .join("; ");

    let code = urlencoding::encode(&format!("{nonce}|{sub}|{email}|{name}")).into_owned();
    let (status, body, headers) = send(
        app,
        get(
            &format!("/api/auth/oidc/callback?code={code}&state={state}"),
            Some(&cookies),
        ),
    )
    .await;
    let session = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("hive_session=") && !v.starts_with("hive_session=;"))
        .map(|v| v.split(';').next().unwrap().to_string());
    SignIn {
        status,
        body,
        session,
    }
}

async fn user_count(store: &Store) -> i64 {
    store.users_count().await.expect("user count")
}

#[tokio::test]
async fn oidc_links_on_issuer_and_subject_and_refuses_everything_else() {
    let issuer = fake_oidc_provider().await;
    std::env::set_var("HIVE_EMBED", "hash");
    std::env::set_var("HIVE_OIDC_ENABLED", "true");
    std::env::set_var("OIDC_ISSUER", &issuer);
    std::env::set_var("OIDC_CLIENT_ID", "hive-client");
    std::env::set_var("OIDC_CLIENT_SECRET", "fake-secret");
    std::env::set_var(
        "OIDC_REDIRECT_URI",
        "http://localhost/api/auth/oidc/callback",
    );
    std::env::set_var("OIDC_ALLOWED_DOMAINS", "example.com");
    std::env::remove_var("OIDC_ORG");

    let test_db = hive_api::db::test_pool().await;
    let store = Store::new(test_db.pool.clone());
    let app = hive_api::routes::router(store.clone());
    let (status, body, _) = send(
        &app,
        post(
            "/api/onboarding",
            json!({
                "instanceName": "Hive",
                "adminName": "nate",
                "adminEmail": "nate@example.com",
                "password": "correct-horse",
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "onboarding: {body}");
    // Every sign-in below is checked against this: a refusal must never
    // leave an account behind, and a re-link must never fork one.
    let mut accounts = user_count(&store).await;

    // ---- 1. A first sign-in provisions, on an allowed domain. ----
    let first = sign_in(&app, "subject-alpha", "alpha@example.com").await;
    assert_eq!(first.status, StatusCode::FOUND, "{}", first.body);
    let session = first.session.clone().expect("session cookie");
    accounts += 1;
    assert_eq!(user_count(&store).await, accounts);

    // ---- 2. `(issuer, subject)` is the join key: the SAME sub arriving with a
    // different email is the same person, and email is just a label. ----
    let again = sign_in(&app, "subject-alpha", "alpha-renamed@example.com").await;
    assert_eq!(again.status, StatusCode::FOUND, "{}", again.body);
    assert_eq!(
        user_count(&store).await,
        accounts,
        "a changed email must not fork a second account"
    );
    let (_, me_first, _) = send(&app, get("/api/auth/me", Some(&session))).await;
    let (_, me_again, _) = send(
        &app,
        get("/api/auth/me", Some(&again.session.clone().unwrap())),
    )
    .await;
    let actor = |body: &str| {
        serde_json::from_str::<Value>(body).expect("json")["user"]["actor"]
            .as_str()
            .expect("actor")
            .to_string()
    };
    assert_eq!(actor(&me_first), actor(&me_again), "same local account");
    // The local email is a label written at provisioning, so it does not
    // follow the provider's — another way of saying it is not a key.
    assert!(store
        .users_by_email("alpha-renamed@example.com")
        .await
        .expect("lookup")
        .is_none());

    // ---- 2b. Provisioning may not ADOPT an existing identity. `name` is an
    // unverified string from the provider; slugified, it used to be handed
    // straight to `people_ensure`, which returns the EXISTING person for a
    // taken slug — so signing up as "Pia" made you the AI called pia. ----
    store
        .people_upsert("pia", "Pia", ActorKind::Ai, Some("nate"))
        .await
        .expect("seed an AI identity");
    let impostor = sign_in_as(&app, "subject-impostor", "impostor@example.com", "Pia").await;
    assert_eq!(impostor.status, StatusCode::FOUND, "{}", impostor.body);
    let (_, me_impostor, _) = send(
        &app,
        get("/api/auth/me", Some(&impostor.session.clone().unwrap())),
    )
    .await;
    assert_ne!(actor(&me_impostor), "pia", "provisioning adopted the AI");
    accounts += 1;
    let pia = store
        .people_get("pia")
        .await
        .expect("pia")
        .expect("pia row");
    assert_eq!(pia.kind, ActorKind::Ai, "and left it an AI");
    assert_eq!(pia.owner.as_deref(), Some("nate"), "still nate's");

    // ---- 3. THE takeover. A DIFFERENT sub arriving with an existing local
    // account's address gets nothing. Registering the victim's address at a
    // trusted IdP must not hand over the victim's account. ----
    sign_in(&app, "subject-attacker", "nate@example.com")
        .await
        .refused("account already exists");
    assert_eq!(
        user_count(&store).await,
        accounts,
        "the refusal must not provision either"
    );
    // …and the admin account still belongs to the admin.
    let (status, body, _) = send(
        &app,
        post(
            "/api/auth/login",
            json!({"email": "nate@example.com", "password": "correct-horse"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "admin can still log in: {body}");
    assert!(body.contains("\"role\":\"admin\""), "{body}");

    // ---- 4. An id_token with no usable `sub` identifies nobody. Refuse it
    // rather than falling back to the one claim that must never be a key. ----
    sign_in(&app, "!missing", "nosub@example.com")
        .await
        .refused("no sub in id_token");
    sign_in(&app, "!empty", "emptysub@example.com")
        .await
        .refused("no sub in id_token");
    assert_eq!(user_count(&store).await, accounts);

    // ---- 5. Auto-provisioning needs an unambiguous org. With a second org and
    // no OIDC_ORG, dropping every new IdP user into `default` would hand them
    // that org's content, so the sign-in is refused instead. ----
    store.orgs_create("beta", "Beta").await.expect("second org");
    sign_in(&app, "subject-beta", "beta-user@example.com")
        .await
        .refused("no unambiguous org");
    assert_eq!(user_count(&store).await, accounts);

    std::env::set_var("OIDC_ORG", "beta");
    let scoped = sign_in(&app, "subject-beta", "beta-user@example.com").await;
    assert_eq!(scoped.status, StatusCode::FOUND, "{}", scoped.body);
    accounts += 1;
    assert_eq!(user_count(&store).await, accounts);
    let beta_user = store
        .users_by_email("beta-user@example.com")
        .await
        .expect("lookup")
        .expect("provisioned")
        .0;
    let orgs = store
        .memberships_for(&beta_user.id)
        .await
        .expect("memberships");
    assert_eq!(orgs.len(), 1);
    assert_eq!(orgs[0].org.slug, "beta", "provisioned into the named org");
    assert_eq!(orgs[0].role, "member");

    // An OIDC_ORG naming an org that does not exist is a refusal, not a
    // fallback to `default`.
    std::env::set_var("OIDC_ORG", "no-such-org");
    sign_in(&app, "subject-gamma", "gamma@example.com")
        .await
        .refused("no unambiguous org");
    std::env::remove_var("OIDC_ORG");

    // ---- 6. A `user_identities` row pointing at a deleted user is a broken
    // instance, not a new sign-up. It used to fall through to provisioning,
    // where `identity_link` (ON CONFLICT DO NOTHING) linked nothing and the
    // NEXT login hit the already-registered-email refusal forever. ----
    let alpha = store
        .users_by_email("alpha@example.com")
        .await
        .expect("lookup")
        .expect("alpha")
        .0;
    hive_api::pgq::query("DELETE FROM users WHERE id = ?")
        .bind(&alpha.id)
        .execute(store.db())
        .await
        .expect("delete the user out from under its identity");
    let orphaned = sign_in(&app, "subject-alpha", "alpha-renamed@example.com").await;
    orphaned.refused("no longer exists");
}
