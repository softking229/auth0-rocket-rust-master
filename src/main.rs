#![feature(plugin)]
#![feature(try_trait)]
#![feature(proc_macro_hygiene, decl_macro)]

#[macro_use]
extern crate failure_derive;

use rocket::{get, routes};
use serde_derive::{Serialize, Deserialize};
use bincode::{deserialize, serialize};
use chrono::Utc;
use crypto_hash::hex_digest;
use crypto_hash::Algorithm as HashAlgorithm;
use failure::Error;
use frank_jwt::{decode, Algorithm};
use keyz::{make_key, Key};
use maud::{html, Markup};
use rocket::config::ConfigError;
use rocket::fairing::AdHoc;
use rocket::http::uri::Uri;
use rocket::http::{Cookie, Cookies, Status};
use rocket::request::{FromRequest, Outcome, Request};
use rocket::response::Redirect;
use rocket::State;
use serde_json::ser::to_vec;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use url::Url;

use errors::*;
use rand::Rng;

// Our own error types.
mod errors;

/// Alias to a sled db wrapped in an Arc smart pointer.
type DB = Arc<sled::Db>;

fn main() {
    let routes = get_routes();
    let db: DB = {
        let cb = sled::ConfigBuilder::new().path(".data").build();
        Arc::new(sled::Db::start(cb).expect("could not open sled db"))
    };
    rocket::ignite()
        .mount("/", routes)
        .manage(db)
        .attach(AdHoc::on_attach("secrets", |rocket: rocket::Rocket| {
            let conf = rocket.config().clone();
            let settings = AuthSettings::from_env(&conf, "AUTH0_CLIENT_SECRET")
                .expect("AUTH0_CLIENT_SECRET must be set in your environment");
            {
                // a call to state() borrows the rocket instance, but we can
                // introduce a lexical scope to limit our temporary borrow.
                let db = rocket.state::<DB>().expect("could not get db state");
                populate_certs(db, &settings.auth0_domain)
                    .map_err(|e| panic!("populate_certs: {:?}", e))
                    .unwrap();
            }
            Ok(rocket.manage(settings))
        }))
        .launch();
}

fn populate_certs(db: &DB, auth0_domain: &str) -> Result<(), Error> {
    let client = reqwest::Client::new();
    let cert_endpoint = format!("https://{}/pem", auth0_domain);
    let pem_cert: String = client
        .get(
            Url::from_str(&cert_endpoint)
                .expect("could not parse auth0_domain")
                .as_str(),
        )
        .send()?
        .text()?;
    // transform cert into X509 struct
    use openssl::x509::X509;
    let cert = X509::from_pem(pem_cert.as_bytes()).expect("x509 parse failed");
    let pk = cert.public_key()?;
    // extract public keys and cert in pem and der
    let pem_pk = pk.public_key_to_pem()?;
    let der_pk = pk.public_key_to_der()?;
    let der_cert = cert.to_der()?;
    // save as bytes to database
    db.set(b"jwt_pub_key_pem".to_vec(), pem_pk).unwrap();
    db.set(b"jwt_pub_key_der".to_vec(), der_pk).unwrap();
    db.set(b"jwt_cert_der".to_vec(), der_cert).unwrap();
    Ok(())
}

fn decode_and_validate_jwt(
    pub_key: Vec<u8>,
    jwt: &str,
    aud: &str,
    auth0_domain: &str,
) -> Result<Auth0JWTPayload, Error> {
    let (_, json) = decode(
        &jwt.to_string(),
        &String::from_utf8(pub_key).expect("pk is not valid UTF-8"),
        Algorithm::RS256,
    )
    .map_err(|_| AuthError::MalformedJWT {
        repr: jwt.to_string(),
    })?;
    let payload = Auth0JWTPayload::from_json(&json)?;
    // We've decoded the jwt payload. Now we validate some fields.
    let now = Utc::now().timestamp();
    if payload.exp < now {
        return Err(AuthError::Expired)?;
    };
    if payload.aud != aud.to_string() {
        return Err(AuthError::AudienceMismatch)?;
    };
    if payload.iss != format!("https://{}/", auth0_domain) {
        return Err(AuthError::IssuerMismatch)?;
    };
    Ok(payload)
}

fn get_or_create_user(db: &DB, jwt: &Auth0JWTPayload) -> Result<User, Error> {
    let user_key = make_key!("users/", jwt.user_id.clone());

    let user = match db.get(&user_key.0)? {
        None => {
            // user was not found, make a new one
            let user = User {
                email: jwt.email.clone(),
                user_id: jwt.user_id.clone(),
            };
            let encoded_user = serialize(&user).map_err(|_| SerializationError {
                name: format!("user"),
            })?;
            db.set(user_key.0, encoded_user).unwrap();
            Ok(user)
        }
        // Some(sled::IVec)
        Some(sled_ivec) => {
            // Turn the db type into a regular vec
            let user_bytes: Vec<u8> = sled_ivec.to_vec();
            let user: User =
                deserialize(user_bytes.as_slice()).map_err(|_| DeserializationError {
                    name: format!("user_bytes"),
                })?;
            Ok(user)
        }
    };
    user
}

fn get_routes() -> Vec<rocket::Route> {
    routes![
        login,
        logged_in,
        auth0_redirect,
        auth0_callback,
        home,
        home_redirect,
        static_files
    ]
}

/// Our login link.
#[get("/login", rank = 2)]
fn login() -> Markup {
    html! {
        head {
            title { "Login | Auth0 Rocket Example" }
            link rel="stylesheet" href="static/css/style.css";
        }
        body {
            a class="login" href="/auth0" {"Login With Auth0!"}
        }
    }
}

/// This route is chosen if the request guard for User passes (e.g. logged in).
#[get("/")]
fn home(user: User, _cookies: Cookies) -> Markup {
    html! {
        head {
            title {"Welcome | Auth0 Rocket Example"}
            link rel="stylesheet" href="static/css/style.css";
        }
        body{
            h1 {"Guarded Route"}
            div {p {
                "You logged in successfully."
            }}
            div {p {
                "Email: " (user.email)
            }}
            div {p {
                a class="login" href="/profile" {"Another private route"}
            }}
        }
    }
}

/// This redirect fires if you go to "/" without being logged in.
#[get("/", rank = 2)]
fn home_redirect() -> Redirect {
    Redirect::to("/login")
}

#[get("/loggedin")]
fn logged_in() -> Redirect {
    Redirect::to("/")
}

/// Serve static files under /static dir.
#[get("/static/<path..>")]
fn static_files(path: PathBuf) -> Option<rocket::response::NamedFile> {
    rocket::response::NamedFile::open(Path::new("static/").join(path)).ok()
}

/// This route reads settings from our application state and redirects to our
/// configured Auth0 login page. If our user's login is successful, Auth0 will
/// redirect them back to /callback with "code" and "state" as query params.
#[get("/auth0")]
fn auth0_redirect(mut cookies: Cookies, settings: State<AuthSettings>) -> Result<Redirect, Status> {
    let state = random_state_string();
    cookies.add(Cookie::new("state", state.clone()));
    let uri = settings.authorize_endpoint_url(&state);
    println!("{:?}", uri);
    use std::convert::TryFrom;
    let redir = Uri::try_from(uri).expect("invalid uri");
    Ok(Redirect::to(redir))
}

/// Login callback. Auth0 sends a request to this endpoint. In the query string
/// we extract the "code" and "state" parameters, ensuring that "state" matches
/// the string we passed to Auth0's /authorize endpoint. Then we can use "code"
/// in a TokenRequest to the /oauth/token endpoint.
#[get("/callback?<code>&<state>")]
fn auth0_callback(
    code: String,
    state: String,
    mut cookies: Cookies,
    db: State<DB>,
    settings: State<AuthSettings>,
) -> Result<Redirect, Status> {
    if let Some(cookie) = cookies.get("state") {
        if state != cookie.value() {
            return Err(rocket::http::Status::Forbidden);
        }
    } else {
        println!("cookie state bad");
        return Err(rocket::http::Status::BadRequest);
    }
    cookies.remove(Cookie::named("state"));

    let tr = settings.token_request(&code);

    // TODO: The call to /oauth/token can panic if there are any misconfigurations: The wrong
    // secret, for instance; also, if the user is unauthorized. We need a nicer way to handle
    // unauthorized here. Also, we need a nicer way to debug the response. We deserialize directly
    // into a TokenResponse, but the auth0 api will return this in the event of misconfiguration:
    //   "{\"error\":\"access_denied\",\"error_description\":\"Unauthorized\"}"
    let token_endpoint = format!("https://{}/oauth/token", settings.auth0_domain);
    println!("token endpoint time: {:?}", token_endpoint);
    let client = reqwest::Client::new();
    let resp: TokenResponse = client
        .post(&token_endpoint)
        .header("Content-Type", "application/json")
        .body(to_vec(&tr).unwrap())
        .send()
        .unwrap()
        .json()
        .expect("could not deserialize response from /oauth/token");

    // TODO: Can we unwrap here because we know for certain we've populated the cert in the db?
    let pub_key: Vec<u8> = db.get(b"jwt_pub_key_pem").unwrap().unwrap().to_vec();
    let payload = decode_and_validate_jwt(
        pub_key,
        &resp.id_token,
        &settings.client_id,
        &settings.auth0_domain,
    )
    .map_err(|_| Status::Unauthorized)?;
    let user = get_or_create_user(&db, &payload).map_err(|e| match e.downcast_ref() {
        Some(AuthError::MalformedJWT { .. }) => Status::BadRequest,
        _ => Status::InternalServerError,
    })?;

    let jwt = &resp.id_token.clone();
    let hashed_jwt = hex_digest(HashAlgorithm::SHA256, jwt.as_bytes());
    let new_session = Session {
        user_id: user.user_id,
        expires: payload.exp,
        raw_jwt: jwt.as_bytes().to_vec(),
    };
    let encoded_session = serialize(&new_session).map_err(|_| Status::Unauthorized)?;
    let session_key = make_key!("sessions/", hashed_jwt.clone());
    db.set(session_key.0, encoded_session).unwrap();
    let cookie = Cookie::build("session", hashed_jwt)
        .path("/")
        .secure(true)
        .http_only(true)
        .finish();
    cookies.add(cookie);

    Ok(Redirect::to("/loggedin"))
}

/// Helper to create a random string 30 chars long.
pub fn random_state_string() -> String {
    use rand::{distributions::Alphanumeric, thread_rng};
    use std::iter;
    let mut rng = thread_rng();

    let random: String = iter::repeat(())
        .map(|()| rng.sample(Alphanumeric))
        .take(7)
        .collect();
    random
}

/// Send TokenRequest to the Auth0 /oauth/token endpoint.
#[derive(Serialize, Deserialize)]
struct TokenRequest {
    grant_type: String,
    client_id: String,
    client_secret: String,
    code: String,
    redirect_uri: String,
}

#[derive(Serialize, Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u32,
    id_token: String,
    token_type: String,
}

/// Configuration state for Auth0, including the client secret, which
/// must be kept private.
struct AuthSettings {
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    auth0_domain: String,
}

/// Holds deserialized data from the /oauth/token endpoint. Use the fields
/// of this struct for validation.
#[derive(Serialize, Deserialize)]
struct Auth0JWTPayload {
    email: String,
    user_id: String,
    exp: i64,
    iss: String,
    aud: String,
}

impl Auth0JWTPayload {
    /// Creates a Auth0JWTPayload from a subset of fields returned as json
    /// from the /oauth/token endpoint.
    pub fn from_json(json: &Value) -> Result<Auth0JWTPayload, Error> {
        match (
            json.get("email"),
            json.get("user_id"),
            json.get("exp"),
            json.get("iss"),
            json.get("aud"),
        ) {
            (Some(email), Some(user_id), Some(exp_str), Some(iss), Some(aud)) => {
                Ok(Auth0JWTPayload {
                    email: email.as_str().unwrap().to_string(),
                    user_id: user_id.as_str().unwrap().to_string(),
                    exp: exp_str.as_i64().unwrap(),
                    iss: iss.as_str().unwrap().to_string(),
                    aud: aud.as_str().unwrap().to_string(),
                })
            }
            _ => Err(AuthError::MalformedJWT {
                repr: format!("{:?}", json.clone()),
            })?,
        }
    }
}

impl AuthSettings {
    /// Constructs an AuthSettings from Rocket.toml and an the client secret
    /// environment variable.
    pub fn from_env(
        conf: &rocket::Config,
        client_secret_env_var: &str,
    ) -> Result<AuthSettings, ConfigError> {
        let app_settings = AuthSettings {
            client_id: String::from(conf.get_str("client_id")?),
            client_secret: std::env::var(client_secret_env_var)
                .map_err(|_| ConfigError::NotFound)?,
            redirect_uri: String::from(conf.get_str("redirect_uri")?),
            auth0_domain: String::from(conf.get_str("auth0_domain")?),
        };
        Ok(app_settings)
    }

    /// Given a state param, build a url String that our /auth0 redirect handler can use.
    pub fn authorize_endpoint_url(&self, state: &str) -> String {
        format!(
            "https://{}/authorize?response_type=code&client_id={}&redirect_uri={}&scope=openid%20profile&state={}",
            self.auth0_domain,
            self.client_id,
            Uri::percent_encode(&self.redirect_uri),
            state,
        )
    }

    /// Builds a TokenRequest from an authorization code and
    /// Auth0 config values.
    pub fn token_request(&self, code: &str) -> TokenRequest {
        TokenRequest {
            grant_type: String::from("authorization_code"),
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
            code: code.to_string(),
            redirect_uri: self.redirect_uri.clone(),
        }
    }
}

/// Session is serialized to the database and retrived to check if
/// a user is logged in.
#[derive(Debug, Serialize, Deserialize)]
struct Session {
    user_id: String,
    expires: i64,
    raw_jwt: Vec<u8>,
}

impl Session {
    /// Check if the session is expired.
    pub fn expired(&self) -> bool {
        let now = Utc::now().timestamp();
        self.expires <= now
    }
}

/// User implements a Rocket request guard that uses a session cookie to
/// look up a Session and User in the database. The guard only passes if
/// the Session is unexpired and the User exists.
#[derive(Debug, Serialize, Deserialize)]
struct User {
    user_id: String,
    email: String,
}

impl<'a, 'r> FromRequest<'a, 'r> for User {
    type Error = ();
    fn from_request(request: &'a Request<'r>) -> Outcome<User, ()> {
        let session_id: Option<String> = request
            .cookies()
            .get("session")
            .and_then(|cookie| cookie.value().parse().ok());
        match session_id {
            None => {
                println!("no session id");
                rocket::Outcome::Forward(())
            }
            Some(session_id) => {
                println!("session id: {}", session_id);
                let db = State::<DB>::from_request(request).unwrap();
                let session_key = make_key!("sessions/", session_id);
                match db.get(&session_key.0) {
                    Ok(Some(sess)) => {
                        let sess: Session =
                            deserialize(&sess).expect("could not deserialize session");
                        if sess.expired() {
                            return rocket::Outcome::Forward(());
                        }
                        let user_key = make_key!("users/", sess.user_id);
                        match db.get(&user_key.0) {
                            Ok(Some(user)) => {
                                let user: User =
                                    deserialize(&user).expect("could not deserialize user");
                                rocket::Outcome::Success(user)
                            }
                            _ => rocket::Outcome::Forward(()),
                        }
                    }
                    _ => rocket::Outcome::Forward(()),
                }
            }
        }
    }
}
