//! The one read the operator makes of AWS: does this EC2 instance still exist.
//!
//! `DescribeInstances` over the EC2 query API, signed with Signature Version 4
//! (`hmac` + `sha2`). The AWS SDK is not used: its dependency tree is larger than
//! this operator's, for one call.
//!
//! Credentials come from the environment, as every other secret reaches the
//! operator: a Secret that Hanzo KMS syncs, mapped to `AWS_ACCESS_KEY_ID`,
//! `AWS_SECRET_ACCESS_KEY` and, for temporary credentials, `AWS_SESSION_TOKEN`.
//! The instance metadata service is no source: the nodes close it to pods (hop
//! limit 1). The policy behind the key is `ec2:DescribeInstances` and nothing
//! else.
//!
//! Neither [`Credentials`] nor [`Ec2`] implements `Debug`, so no log line can
//! carry a key.

use std::time::Duration;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::core::{OperatorError, Result};

type HmacSha256 = Hmac<Sha256>;

const SERVICE: &str = "ec2";
const API_VERSION: &str = "2016-11-15";
const CONTENT_TYPE: &str = "application/x-www-form-urlencoded; charset=utf-8";
const TIMEOUT: Duration = Duration::from_secs(10);

const KEY_ID: &str = "AWS_ACCESS_KEY_ID";
const KEY_SECRET: &str = "AWS_SECRET_ACCESS_KEY";
const KEY_TOKEN: &str = "AWS_SESSION_TOKEN";

/// The error code EC2 answers for an instance it has no record of. AWS keeps a
/// terminated instance visible for about an hour, then forgets it.
const NOT_FOUND: &str = "InvalidInstanceID.NotFound";

/// An access key pair, and the session token when the key is temporary.
pub struct Credentials {
    id: String,
    secret: String,
    token: Option<String>,
}

impl Credentials {
    pub fn new(id: &str, secret: &str, token: Option<&str>) -> Self {
        Self {
            id: id.to_string(),
            secret: secret.to_string(),
            token: token.map(str::to_string),
        }
    }

    /// The key pair in the process environment.
    pub fn from_env() -> std::result::Result<Option<Self>, &'static str> {
        Self::read(|name| std::env::var(name).ok())
    }

    /// The key pair `get` finds, by the names the AWS tools use. `Ok(None)`:
    /// neither half is set (empty counts as unset). `Err(name)`: one half is,
    /// and `name` is the half that is not, which is a Secret mapped in part and
    /// not an absence anyone chose.
    pub fn read(
        get: impl Fn(&str) -> Option<String>,
    ) -> std::result::Result<Option<Self>, &'static str> {
        let var = |name: &str| get(name).filter(|v| !v.is_empty());
        match (var(KEY_ID), var(KEY_SECRET)) {
            (Some(id), Some(secret)) => {
                Ok(Some(Self::new(&id, &secret, var(KEY_TOKEN).as_deref())))
            }
            (None, None) => Ok(None),
            (Some(_), None) => Err(KEY_SECRET),
            (None, Some(_)) => Err(KEY_ID),
        }
    }
}

/// An EC2 instance, named the way a Kubernetes Node names it: its providerID,
/// `aws:///<zone>/<instance id>`. Only a well-formed one parses, so what reaches
/// a request is a region and a hex id, never free text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instance {
    region: String,
    id: String,
}

impl Instance {
    pub fn from_provider_id(provider_id: &str) -> Option<Self> {
        let (zone, id) = provider_id.strip_prefix("aws:///")?.split_once('/')?;
        // `us-east-1a` is region `us-east-1` and a zone letter. A zone that does
        // not reduce to that shape (a local zone is `us-east-1-bos-1a`) names no
        // region this can ask, so it parses to nothing.
        let region = zone.strip_suffix(|c: char| c.is_ascii_lowercase())?;
        let mut parts = region.split('-');
        let last = parts.next_back()?;
        let shaped = !last.is_empty()
            && last.bytes().all(|b| b.is_ascii_digit())
            && parts.clone().count() >= 1
            && parts.all(|p| p.len() >= 2 && p.bytes().all(|b| b.is_ascii_lowercase()));
        let hex = id.strip_prefix("i-")?;
        let hex_id = !hex.is_empty() && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        (shaped && hex_id).then(|| Self {
            region: region.to_string(),
            id: id.to_string(),
        })
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

/// A signed client for `DescribeInstances`.
pub struct Ec2 {
    credentials: Credentials,
    http: reqwest::Client,
    /// `None`: the region's own endpoint, always, outside tests. A test points it
    /// at a stand-in for AWS.
    endpoint: Option<reqwest::Url>,
}

impl Ec2 {
    pub fn new(credentials: Credentials) -> Self {
        Self {
            credentials,
            http: reqwest::Client::builder()
                .timeout(TIMEOUT)
                .build()
                .expect("build the HTTP client"),
            endpoint: None,
        }
    }

    /// A client that sends every request to `endpoint` instead of AWS.
    #[cfg(test)]
    pub fn at(credentials: Credentials, endpoint: reqwest::Url) -> Self {
        Self {
            endpoint: Some(endpoint),
            ..Self::new(credentials)
        }
    }

    /// Whether `instance` is gone for good: EC2 reports it `terminated`, or no
    /// longer knows it. Every other state is `false`, and so is none of the
    /// above: an answer this cannot read, a refusal, a timeout, an unreachable
    /// AWS are all errors, so a caller that deletes on `true` deletes only on
    /// AWS's word.
    pub async fn gone(&self, instance: &Instance) -> Result<bool> {
        let url = match &self.endpoint {
            Some(url) => url.clone(),
            None => format!("https://ec2.{}.amazonaws.com/", instance.region)
                .parse()
                .map_err(|e| OperatorError::Aws(format!("endpoint: {e}")))?,
        };
        let host = match (url.host_str(), url.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            (None, _) => return Err(OperatorError::Aws("endpoint has no host".to_string())),
        };
        let body = format!(
            "Action=DescribeInstances&InstanceId.1={}&Version={API_VERSION}",
            instance.id
        );
        let stamp = jiff::Timestamp::now()
            .strftime("%Y%m%dT%H%M%SZ")
            .to_string();

        let mut headers = vec![
            ("content-type", CONTENT_TYPE),
            ("host", host.as_str()),
            ("x-amz-date", stamp.as_str()),
        ];
        if let Some(token) = &self.credentials.token {
            headers.push(("x-amz-security-token", token.as_str()));
        }
        let (canonical, signed) = canonical("POST", url.path(), "", &headers, body.as_bytes());
        let signature = signature(
            &self.credentials.secret,
            &stamp,
            &instance.region,
            SERVICE,
            &canonical,
        );
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}/{}/{SERVICE}/aws4_request, SignedHeaders={signed}, Signature={signature}",
            self.credentials.id,
            &stamp[..8],
            instance.region,
        );

        let mut request = self
            .http
            .post(url)
            .header("content-type", CONTENT_TYPE)
            .header("x-amz-date", &stamp)
            .header("authorization", authorization);
        if let Some(token) = &self.credentials.token {
            request = request.header("x-amz-security-token", token);
        }
        let response = request.body(body).send().await?;
        let status = response.status();
        let xml = response.text().await?;
        decide(status.as_u16(), &xml, &instance.id)
    }
}

/// The meaning of EC2's answer for `id`. Pure.
fn decide(status: u16, xml: &str, id: &str) -> Result<bool> {
    if (200..300).contains(&status) {
        // The answer must be about the instance asked for.
        let state = (text(xml, "instanceId") == Some(id))
            .then(|| text(xml, "instanceState").and_then(|s| text(s, "name")))
            .flatten();
        return match state {
            Some(name) => Ok(name == "terminated"),
            None => Err(OperatorError::Aws(format!(
                "DescribeInstances {status}: no state for {id}"
            ))),
        };
    }
    match text(xml, "Code") {
        Some(NOT_FOUND) => Ok(true),
        Some(code) => Err(OperatorError::Aws(format!(
            "DescribeInstances {status}: {code}"
        ))),
        None => Err(OperatorError::Aws(format!("DescribeInstances {status}"))),
    }
}

/// The text of the first `<tag>…</tag>` in `xml`.
fn text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&format!("</{tag}>"))?;
    Some(&xml[start..end])
}

/// The canonical request of Signature Version 4, and its signed-header list.
/// `headers` are lowercase names with the values as sent.
fn canonical(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (String, String) {
    let mut sorted = headers.to_vec();
    sorted.sort();
    let names = sorted
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(";");
    let lines: String = sorted
        .iter()
        .map(|(name, value)| format!("{name}:{}\n", value.trim()))
        .collect();
    let request = format!(
        "{method}\n{path}\n{query}\n{lines}\n{names}\n{:x}",
        Sha256::digest(body)
    );
    (request, names)
}

/// The hex signature of `canonical` for the request time `stamp`
/// (`YYYYMMDDTHHMMSSZ`).
fn signature(secret: &str, stamp: &str, region: &str, service: &str, canonical: &str) -> String {
    let date = &stamp[..8];
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{stamp}\n{date}/{region}/{service}/aws4_request\n{:x}",
        Sha256::digest(canonical)
    );
    let mut key = mac(format!("AWS4{secret}").as_bytes(), date);
    for part in [region, service, "aws4_request"] {
        key = mac(&key, part);
    }
    mac(&key, &to_sign)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn mac(key: &[u8], data: &str) -> Vec<u8> {
    // HMAC takes a key of any length; `new_from_slice` fails only for MACs with a
    // fixed key size.
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the signature ----------------------------------------------------

    /// The example AWS documents for Signature Version 4 (IAM `ListUsers`,
    /// 2015-08-30T12:36:00Z, us-east-1, access key AKIDEXAMPLE): the canonical
    /// request's hash, and the signature over it. A signer that is wrong in any
    /// step (header order, date scope, key derivation) does not reproduce them.
    #[test]
    fn the_signature_is_the_one_aws_documents() {
        let headers = [
            ("host", "iam.amazonaws.com"),
            ("content-type", CONTENT_TYPE),
            ("x-amz-date", "20150830T123600Z"),
        ];
        let (canonical, signed) = canonical(
            "GET",
            "/",
            "Action=ListUsers&Version=2010-05-08",
            &headers,
            b"",
        );
        assert_eq!(signed, "content-type;host;x-amz-date");
        assert_eq!(
            format!("{:x}", Sha256::digest(&canonical)),
            "f536975d06c0309214f805bb90ccff089219ecd68b2577efef23edd43b7e1a59"
        );
        assert_eq!(
            signature(
                "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
                "20150830T123600Z",
                "us-east-1",
                "iam",
                &canonical,
            ),
            "5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    #[test]
    fn a_session_token_is_a_signed_header() {
        let headers = [
            ("x-amz-security-token", "tok"),
            ("host", "ec2.us-east-1.amazonaws.com"),
            ("x-amz-date", "20260101T000000Z"),
            ("content-type", CONTENT_TYPE),
        ];
        let (_, signed) = canonical("POST", "/", "", &headers, b"x");
        assert_eq!(signed, "content-type;host;x-amz-date;x-amz-security-token");
    }

    // ---- the credentials --------------------------------------------------

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn both_halves_of_the_key_make_credentials() {
        let c = Credentials::read(env(&[
            ("AWS_ACCESS_KEY_ID", "id"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            (c.id.as_str(), c.secret.as_str(), &c.token),
            ("id", "secret", &None)
        );

        let c = Credentials::read(env(&[
            ("AWS_ACCESS_KEY_ID", "id"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("AWS_SESSION_TOKEN", "tok"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(c.token.as_deref(), Some("tok"));

        // An empty token is no token.
        let c = Credentials::read(env(&[
            ("AWS_ACCESS_KEY_ID", "id"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("AWS_SESSION_TOKEN", ""),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(c.token, None);
    }

    #[test]
    fn no_key_is_not_an_error_and_half_a_key_names_the_missing_half() {
        assert!(Credentials::read(env(&[])).unwrap().is_none());
        // Empty is unset.
        assert!(Credentials::read(env(&[
            ("AWS_ACCESS_KEY_ID", ""),
            ("AWS_SECRET_ACCESS_KEY", ""),
        ]))
        .unwrap()
        .is_none());
        assert_eq!(
            Credentials::read(env(&[("AWS_ACCESS_KEY_ID", "id")])).err(),
            Some("AWS_SECRET_ACCESS_KEY")
        );
        assert_eq!(
            Credentials::read(env(&[
                ("AWS_ACCESS_KEY_ID", ""),
                ("AWS_SECRET_ACCESS_KEY", "secret"),
            ]))
            .err(),
            Some("AWS_ACCESS_KEY_ID")
        );
    }

    // ---- the instance -----------------------------------------------------

    #[test]
    fn a_provider_id_names_a_region_and_an_instance() {
        let i = Instance::from_provider_id("aws:///us-east-1a/i-00e2538fddd576f9e").unwrap();
        assert_eq!(i.region(), "us-east-1");
        assert_eq!(i.id(), "i-00e2538fddd576f9e");
        let i = Instance::from_provider_id("aws:///ap-southeast-2c/i-0abc1234").unwrap();
        assert_eq!(i.region(), "ap-southeast-2");
        let i = Instance::from_provider_id("aws:///us-gov-west-1b/i-0abc1234").unwrap();
        assert_eq!(i.region(), "us-gov-west-1");
    }

    #[test]
    fn what_is_not_an_ec2_provider_id_is_no_instance() {
        for bad in [
            "",
            "k3s://ip-10-64-1-10",
            "aws://us-east-1a/i-00e2538fddd576f9e",
            "aws:///us-east-1a",
            "aws:///us-east-1a/",
            "aws:///us-east-1a/i-",
            "aws:///us-east-1a/i-XYZ",
            "aws:///us-east-1a/i-0abc&Action=TerminateInstances",
            "aws:///us-east-1a/vol-0abc1234",
            "aws:///us-east-1a/i-0abc1234/extra",
            "aws:///us-east-1/i-0abc1234",
            "aws:///us-east-1-bos-1a/i-0abc1234",
            "aws:///1a/i-0abc1234",
            "aws:///-1a/i-0abc1234",
            "aws:///us-east-a/i-0abc1234",
            "aws:///US-EAST-1A/i-0abc1234",
        ] {
            assert_eq!(Instance::from_provider_id(bad), None, "{bad:?}");
        }
    }

    // ---- the answer -------------------------------------------------------

    fn described(id: &str, state: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <DescribeInstancesResponse xmlns=\"http://ec2.amazonaws.com/doc/2016-11-15/\">\
             <requestId>r</requestId><reservationSet><item><reservationId>r-1</reservationId>\
             <instancesSet><item><instanceId>{id}</instanceId><imageId>ami-1</imageId>\
             <instanceState><code>0</code><name>{state}</name></instanceState>\
             <privateDnsName>ip-10-64-2-119</privateDnsName></item></instancesSet>\
             </item></reservationSet></DescribeInstancesResponse>"
        )
    }

    fn refused(code: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Response><Errors><Error>\
             <Code>{code}</Code><Message>m</Message></Error></Errors><RequestID>r</RequestID></Response>"
        )
    }

    #[test]
    fn only_terminated_is_gone() {
        let id = "i-0abc1234";
        assert!(decide(200, &described(id, "terminated"), id).unwrap());
        for state in ["pending", "running", "shutting-down", "stopping", "stopped"] {
            assert!(!decide(200, &described(id, state), id).unwrap(), "{state}");
        }
    }

    #[test]
    fn an_instance_ec2_has_forgotten_is_gone() {
        let id = "i-0abc1234";
        assert!(decide(400, &refused("InvalidInstanceID.NotFound"), id).unwrap());
    }

    #[test]
    fn an_answer_that_is_no_verdict_is_an_error() {
        let id = "i-0abc1234";
        // Refusals and throttling say nothing of the instance.
        for (status, code) in [
            (403, "UnauthorizedOperation"),
            (401, "AuthFailure"),
            (403, "InvalidClientTokenId"),
            (503, "RequestLimitExceeded"),
            (400, "InvalidInstanceID.Malformed"),
        ] {
            assert!(decide(status, &refused(code), id).is_err(), "{code}");
        }
        // Nothing parseable.
        assert!(decide(500, "", id).is_err());
        assert!(decide(502, "<html>bad gateway</html>", id).is_err());
        assert!(decide(200, "", id).is_err());
        assert!(decide(200, "<DescribeInstancesResponse/>", id).is_err());
        // An answer about another instance is no answer about this one.
        assert!(decide(200, &described("i-0other", "terminated"), id).is_err());
    }

    // ---- the request, over HTTP --------------------------------------------

    mod wire {
        use axum::extract::State;
        use axum::http::HeaderMap;
        use axum::routing::post;
        use axum::Router;
        use std::sync::{Arc, Mutex};

        pub struct Seen {
            pub headers: HeaderMap,
            pub body: String,
        }

        /// A stand-in EC2 that records each request and answers `reply`.
        pub async fn serve(status: u16, reply: String) -> (reqwest::Url, Arc<Mutex<Vec<Seen>>>) {
            #[derive(Clone)]
            struct App {
                status: u16,
                reply: String,
                seen: Arc<Mutex<Vec<Seen>>>,
            }
            async fn handle(
                State(app): State<App>,
                headers: HeaderMap,
                body: String,
            ) -> (axum::http::StatusCode, String) {
                app.seen.lock().unwrap().push(Seen { headers, body });
                (
                    axum::http::StatusCode::from_u16(app.status).unwrap(),
                    app.reply.clone(),
                )
            }
            let seen = Arc::new(Mutex::new(Vec::new()));
            let app = Router::new().route("/", post(handle)).with_state(App {
                status,
                reply,
                seen: seen.clone(),
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let _ = axum::serve(listener, app.into_make_service()).await;
            });
            (format!("http://{addr}/").parse().unwrap(), seen)
        }
    }

    fn client_to(url: reqwest::Url, token: Option<&str>) -> Ec2 {
        // The install `main` makes before any TLS client is built; a lib test
        // never runs `main`.
        static PROVIDER: std::sync::Once = std::sync::Once::new();
        PROVIDER.call_once(|| {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        });
        Ec2::at(
            Credentials::new(
                "AKIDEXAMPLE",
                "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
                token,
            ),
            url,
        )
    }

    #[tokio::test]
    async fn the_request_asks_for_one_instance_and_is_signed() {
        let id = "i-00e2538fddd576f9e";
        let (url, seen) = wire::serve(200, described(id, "terminated")).await;
        let instance = Instance::from_provider_id(&format!("aws:///us-east-1a/{id}")).unwrap();
        assert!(client_to(url, None).gone(&instance).await.unwrap());

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].body,
            format!("Action=DescribeInstances&InstanceId.1={id}&Version=2016-11-15")
        );
        let auth = seen[0].headers["authorization"].to_str().unwrap();
        let date = seen[0].headers["x-amz-date"].to_str().unwrap();
        // YYYYMMDDTHHMMSSZ, in UTC.
        assert_eq!(date.len(), 16, "{date}");
        assert!(date.ends_with('Z') && date.as_bytes()[8] == b'T', "{date}");
        assert!(
            date.bytes().filter(|b| b.is_ascii_digit()).count() == 14,
            "{date}"
        );
        assert!(date.starts_with("20"), "{date}");
        assert!(
            auth.starts_with(&format!(
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/{}/us-east-1/ec2/aws4_request, \
             SignedHeaders=content-type;host;x-amz-date, Signature=",
                &date[..8]
            )),
            "{auth}"
        );
        assert!(seen[0].headers.get("x-amz-security-token").is_none());
        // 64 hex digits of signature, and the secret nowhere in the request.
        let sig = auth.rsplit("Signature=").next().unwrap();
        assert_eq!(sig.len(), 64);
        assert!(sig.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(!auth.contains("wJalrXUtnFEMI"));
    }

    #[tokio::test]
    async fn a_session_token_rides_the_request_and_the_signature() {
        let id = "i-0abc1234";
        let (url, seen) = wire::serve(200, described(id, "running")).await;
        let instance = Instance::from_provider_id(&format!("aws:///us-east-1a/{id}")).unwrap();
        assert!(!client_to(url, Some("session"))
            .gone(&instance)
            .await
            .unwrap());
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].headers["x-amz-security-token"], "session");
        let auth = seen[0].headers["authorization"].to_str().unwrap();
        assert!(auth.contains("SignedHeaders=content-type;host;x-amz-date;x-amz-security-token"));
    }

    #[tokio::test]
    async fn a_refusal_over_http_is_an_error_and_not_found_is_gone() {
        let id = "i-0abc1234";
        let instance = Instance::from_provider_id(&format!("aws:///us-east-1a/{id}")).unwrap();

        let (url, _) = wire::serve(403, refused("UnauthorizedOperation")).await;
        assert!(client_to(url, None).gone(&instance).await.is_err());

        let (url, _) = wire::serve(400, refused("InvalidInstanceID.NotFound")).await;
        assert!(client_to(url, None).gone(&instance).await.unwrap());
    }
}
