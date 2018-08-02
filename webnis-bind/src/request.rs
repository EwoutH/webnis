
use std::io;
use std::time::{Instant,Duration};
use std::sync::atomic::Ordering;

use url::percent_encoding::{
    utf8_percent_encode,
    DEFAULT_ENCODE_SET,
    QUERY_ENCODE_SET
};
use hyper;
use hyper::client::HttpConnector;
use hyper_tls::HttpsConnector;
use tokio::prelude::*;
use tokio::timer::Delay;
use futures::future;

use super::Context;
use super::response::Response;

/// Possible requests our clients can send us
pub(crate) struct Request<'a> {
    cmd:    Cmd,
    args:   Vec<&'a str>,
}

pub(crate) fn process(ctx: Context, line: String) -> Box<Future<Item=String, Error=io::Error> + Send> {
    let request = match Request::parse(&line) {
        Ok(req) => req,
        Err(e) => return Box::new(future::ok(Response::error(400, &e))),
    };

    let (map, param) = match request.cmd {
        Cmd::GetPwNam => ("passwd", "name"),
        Cmd::GetPwUid => ("passwd", "uid"),
        Cmd::GetGrNam => ("group", "name"),
        Cmd::GetGrGid => ("group", "gid"),
        Cmd::GetGidList => ("gidlist", "name"),
    };
    let path = build_path(&ctx.config, map, param, &request.args[0]);

    get_with_retries(&ctx, path, 0)
}

// build a path from a domain, map, key, value.
fn build_path(cfg: &super::config::Config, map: &str, key: &str, val: &str) -> String {
    if let Some(ref dom) = cfg.domain {
        format!("/{}/{}?{}={}",
                utf8_percent_encode(dom, DEFAULT_ENCODE_SET),
                utf8_percent_encode(map, DEFAULT_ENCODE_SET),
                utf8_percent_encode(key, QUERY_ENCODE_SET),
                utf8_percent_encode(val, QUERY_ENCODE_SET))
    } else {
        format!("/{}?{}={}",
                utf8_percent_encode(map, DEFAULT_ENCODE_SET),
                utf8_percent_encode(key, QUERY_ENCODE_SET),
                utf8_percent_encode(val, QUERY_ENCODE_SET))
    }
}

// build a hyper::Uri from a host and a path.
//
// host can be "hostname", "hostname:port", or "http(s)://hostname".
// if it's in the plain "hostname" format, the scheme will be http is
// the host is localhost, https otherwise.
fn build_uri(host: &str, path: &str) -> hyper::Uri {
    let url = if host.starts_with("http://") || host.starts_with("https://") {
        let host = if host.ends_with("/") {
            &host[0..host.len() - 1]
        } else {
            host
        };
        format!("{}{}", host, path)
    } else if host == "localhost" || host.starts_with("localhost:") {
        format!("http://{}/webnis{}", host, path)
    } else {
        format!("https://{}/webnis{}", host, path)
    };
    url.parse::<hyper::Uri>().unwrap()
}

// build a new hyper::Client.
fn new_client(config: &super::config::Config) -> hyper::Client<HttpsConnector<HttpConnector>> {
    let http2_only = config.http2_only.unwrap_or(false);
    let https = HttpsConnector::new(4).unwrap();
    hyper::Client::builder()
                .http2_only(http2_only)
                .keep_alive(true)
                .keep_alive_timeout(Duration::new(30, 0))
                .build::<_, hyper::Body>(https)
}

// This function can call itself recursively to keep on
// generating futures so as to retry.
//
// On errors (except 404) we cycle to the next server.
//
// If there is a serious error from hyper::Client that we do not reckognize,
// we throw away the current hyper::Client instance and create a new one.
//
// This guards against bugs in hyper::Client or its dependencies
// that can get a hyper::Client stuck, see:
//
// https://github.com/hyperium/hyper/issues/1422
// https://github.com/rust-lang/rust/issues/47955
//
fn get_with_retries(ctx: &Context, path: String, n_retries: u32) -> Box<Future<Item=String, Error=io::Error> + Send> {

    let ctx_clone = ctx.clone();

    let (client, seqno) = {
        let mut guard = ctx.http_client.lock().unwrap();
        let http_client = &mut *guard;
        if http_client.client.is_none() {
            // create a new http client.
            http_client.client.get_or_insert_with(|| new_client(&ctx.config));
            http_client.seqno += 1;
        }
        let cc = http_client.client.as_ref().unwrap().clone();
        (cc, http_client.seqno)
    };

    // build the uri based on the currently active webnis server.
    let server = &ctx.config.servers[seqno % ctx.config.servers.len()];
    let uri = build_uri(server, &path);

    let body = client.get(uri)
    .map_err(|e| {
        // something went very wrong. mark it with code 550 so that at the
        // end of the future chain we can detect it and retry.
        //
        // FIXME differ between real problems where we need to throw away the
        // hyper::Client and problems where we just need to switch to the next server.
        debug!("client: got error, need retry: {}", e);
        Response::error(550, &format!("GET error: {}", e))
    })
    .and_then(|res| {
        // see if response is what we expected
        let is_json = res.headers().get("content-type").map(|h| h == "application/json").unwrap_or(false);
        if !is_json {
            if res.status().is_success() {
                future::err(Response::error(416, "expected application/json"))
            } else {
                let code = res.status().as_u16() as i64;
                future::err(Response::error(code, "HTTP error"))
            }
        } else {
            future::ok(res)
        }
    })
    .and_then(|res| {
        res
        .into_body()
        .concat2()
        .map_err(|_| Response::error(400, "GET body error"))
    });

    // add a timeout. need to have an answer in 1 second.
    let timeout = Instant::now() + Duration::from_millis(1000);
    let body_tmout_wrapper = body.deadline(timeout).map_err(|e| {
        debug!("timeout wrapper: error on {}", e);
        match e.into_inner() {
            Some(e) => e,
            None => Response::error(408, "request timeout"),
        }
    });

    let resp =
    body_tmout_wrapper.then(move |res| {
        let body = match res {
            Ok(body) => body,
            Err(e) => {
                if !e.starts_with("404 ") && !ctx_clone.eof.load(Ordering::SeqCst) && n_retries < 8 {
                    {
    				    let mut guard = ctx_clone.http_client.lock().unwrap();
                        if (*guard).seqno == seqno {
                            // only do something if noone else took action.
                            debug!("invalidating server {} and scheduling retry {} because of {}",
                                   ctx_clone.config.servers[seqno % ctx_clone.config.servers.len()], n_retries + 1, e);
                            if e.starts_with("550 ") {
                                // throw away hyper::Client
    				            (*guard).client.take();
                            } else {
                                // just switch to next server.
                                (*guard).seqno += 1;
                            }
                        } else {
                            debug!("scheduling retry {} because of {}", n_retries + 1, e);
                        }
                    }
					// and retry.
                    return get_with_retries(&ctx_clone, path, n_retries + 1);
                } else {
                    return Box::new(future::ok(e));
                }
            },
        };
        Box::new(future::ok(Response::transform(body)))
    });

    if n_retries > 0 {
        let when = Instant::now() + Duration::from_millis(250);
        Box::new(Delay::new(when).then(move |_| resp))
    } else {
        Box::new(resp)
    }
}

pub(crate) enum Cmd {
    GetPwNam,
    GetPwUid,
    GetGrNam,
    GetGrGid,
    GetGidList,
}

// over-engineered way to lowercase a string without allocating.
fn tolower<'a>(s: &'a str, buf: &'a mut [u8]) -> &'a str {
    let b = s.as_bytes();
    if b.len() > buf.len() {
        return s;
    }
    for idx in 0 .. b.len() {
        let c = b[idx];
        buf[idx] = if c >= 65 && c <= 90 { c + 32 } else { c };
    }
    match ::std::str::from_utf8(&buf[0..b.len()]) {
        Ok(s) => s,
        Err(_) => s,
    }
}

impl<'a> Request<'a> {
    pub fn parse(input: &'a str) -> Result<Request<'a>, String> {
        let mut parts = input.splitn(3, " ");
        let mut buf = [0u8; 16];
	    let c = match parts.next() {
		    None => return Err("NO".to_owned()),
            Some(c) => tolower(c, &mut buf),
        };
        let args = parts.collect::<Vec<_>>();
        let (cmd, nargs) = match c {
            //"auth" => (Cmd::Auth, 3),
            "getpwnam" => (Cmd::GetPwNam, 1),
            "getpwuid" => (Cmd::GetPwUid, 1),
            "getgrnam" => (Cmd::GetGrNam, 1),
            "getgrgid" => (Cmd::GetGrGid, 1),
            "getgidlist" => (Cmd::GetGidList, 1),
            _ => return Err(format!("unknown command {}", c)),
        };
        if nargs != args.len() {
            return Err(format!("{} needs {} arguments", c, nargs));
        }
        Ok(Request{ cmd: cmd, args: args })
    }
}
