use self::auth::{AuthResult, Authenticator};
use self::search::Search;
use self::subs::{
    collections_list, download_folder, get_folder, recent, search, send_file, send_file_simple,
    short_response_boxed, transcodings_list, ResponseFuture, NOT_FOUND_MESSAGE,
};
use self::transcode::QualityLevel;
use self::types::FoldersOrdering;
use crate::config::get_config;
use crate::util::header2header;
use futures::prelude::*;
use futures::{future, TryFutureExt};
use headers::{
    AccessControlAllowCredentials, AccessControlAllowOrigin, HeaderMapExt, Origin, Range,
};
use hyper::service::Service;
use hyper::{Body, Method, Request, Response, StatusCode};
use percent_encoding::percent_decode;
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::task::Poll;
use url::form_urlencoded;

pub mod audio_folder;
pub mod audio_meta;
pub mod auth;
#[cfg(feature = "shared-positions")]
pub mod position;
pub mod search;
mod subs;
pub mod transcode;
mod types;

const APP_STATIC_FILES_CACHE_AGE: u32 = 30 * 24 * 3600;
const FOLDER_INFO_FILES_CACHE_AGE: u32 = 24 * 3600;

type Counter = Arc<AtomicUsize>;

#[derive(Clone)]
pub struct TranscodingDetails {
    pub transcodings: Counter,
    pub max_transcodings: u32,
}

#[derive(Clone)]
pub struct FileSendService<T> {
    pub authenticator: Option<Arc<Box<dyn Authenticator<Credentials = T>>>>,
    pub search: Search<String>,
    pub transcoding: TranscodingDetails,
}

// use only on checked prefixes
fn get_subpath(path: &str, prefix: &str) -> PathBuf {
    Path::new(&path).strip_prefix(prefix).unwrap().to_path_buf()
}

fn add_cors_headers(
    mut resp: Response<Body>,
    origin: Option<Origin>,
    enabled: bool,
) -> Response<Body> {
    if !enabled {
        return resp;
    }
    match origin {
        Some(o) => {
            if let Ok(allowed_origin) = header2header::<_, AccessControlAllowOrigin>(o) {
                let headers = resp.headers_mut();
                headers.typed_insert(allowed_origin);
                headers.typed_insert(AccessControlAllowCredentials);
            }
            resp
        }
        None => resp,
    }
}

#[allow(clippy::type_complexity)]
impl<C: 'static> Service<Request<Body>> for FileSendService<C> {
    type Response = Response<Body>;
    type Error = crate::error::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        //static files
        if req.uri().path() == "/" {
            return send_file_simple(
                &get_config().client_dir,
                "index.html",
                Some(APP_STATIC_FILES_CACHE_AGE),
            );
        };
        if req.uri().path() == "/bundle.js" {
            return send_file_simple(
                &get_config().client_dir,
                "bundle.js",
                Some(APP_STATIC_FILES_CACHE_AGE),
            );
        }
        // from here everything must be authenticated
        let searcher = self.search.clone();
        let transcoding = self.transcoding.clone();
        let cors = get_config().cors;
        let origin = req.headers().typed_get::<Origin>();

        let resp = match self.authenticator {
            Some(ref auth) => {
                Box::pin(auth.authenticate(req).and_then(move |result| match result {
                    AuthResult::Authenticated { request, .. } => {
                        FileSendService::<C>::process_checked(request, searcher, transcoding)
                    }
                    AuthResult::LoggedIn(resp) | AuthResult::Rejected(resp) => {
                        Box::pin(future::ok(resp))
                    }
                }))
            }
            None => FileSendService::<C>::process_checked(req, searcher, transcoding),
        };
        Box::pin(resp.map_ok(move |r| add_cors_headers(r, origin, cors)))
    }
}

impl<C> FileSendService<C> {
    fn process_checked(
        req: Request<Body>,
        searcher: Search<String>,
        transcoding: TranscodingDetails,
    ) -> ResponseFuture {
        let params = req
            .uri()
            .query()
            .map(|query| form_urlencoded::parse(query.as_bytes()).collect::<HashMap<_, _>>());

        match *req.method() {
            Method::GET => {
                let path = match percent_decode(req.uri().path().as_bytes()).decode_utf8() {
                    Ok(s) => s.into_owned(),
                    Err(e) => {
                        error!("Decoded path {} is not UTF8: {}", req.uri().path(), e);
                        return short_response_boxed(StatusCode::NOT_FOUND, NOT_FOUND_MESSAGE);
                    }
                };

                if path.starts_with("/collections") {
                    collections_list()
                } else if path.starts_with("/transcodings") {
                    transcodings_list()
                } else if cfg!(feature = "shared-positions") && path.starts_with("/position") {
                    #[cfg(not(feature = "shared-positions"))]
                    unimplemented!();
                    #[cfg(feature = "shared-positions")]
                    self::position::position_service(req)
                } else {
                    let (path, colllection_index) = match extract_collection_number(path) {
                        Ok(r) => r,
                        Err(_) => {
                            error!("Invalid collection number");
                            return short_response_boxed(StatusCode::NOT_FOUND, NOT_FOUND_MESSAGE);
                        }
                    };

                    let base_dir = &get_config().base_dirs[colllection_index];
                    let ord = params
                        .as_ref()
                        .and_then(|p| p.get("ord").map(|l| FoldersOrdering::from_letter(l)))
                        .unwrap_or(FoldersOrdering::Alphabetical);
                    if path.starts_with("/audio/") {
                        FileSendService::<C>::serve_audio(
                            &req,
                            base_dir,
                            &path,
                            transcoding,
                            params,
                        )
                    } else if path.starts_with("/folder/") {
                        get_folder(base_dir, get_subpath(&path, "/folder/"), ord)
                    } else if !get_config().disable_folder_download && path.starts_with("/download")
                    {
                        download_folder(base_dir, get_subpath(&path, "/download/"))
                    } else if path == "/search" {
                        if let Some(search_string) = params.and_then(|mut p| p.remove("q")) {
                            search(colllection_index, searcher, search_string.into_owned(), ord)
                        } else {
                            error!("q parameter is missing in search");
                            short_response_boxed(StatusCode::NOT_FOUND, NOT_FOUND_MESSAGE)
                        }
                    } else if path.starts_with("/recent") {
                        recent(colllection_index, searcher)
                    } else if path.starts_with("/cover/") {
                        send_file_simple(
                            base_dir,
                            get_subpath(&path, "/cover"),
                            Some(FOLDER_INFO_FILES_CACHE_AGE),
                        )
                    } else if path.starts_with("/desc/") {
                        send_file_simple(
                            base_dir,
                            get_subpath(&path, "/desc"),
                            Some(FOLDER_INFO_FILES_CACHE_AGE),
                        )
                    } else {
                        error!("Invalid path requested {}", path);
                        short_response_boxed(StatusCode::NOT_FOUND, NOT_FOUND_MESSAGE)
                    }
                }
            }

            _ => short_response_boxed(StatusCode::METHOD_NOT_ALLOWED, "Method not supported"),
        }
    }

    fn serve_audio(
        req: &Request<Body>,
        base_dir: &'static Path,
        path: &str,
        transcoding: TranscodingDetails,
        mut params: Option<HashMap<std::borrow::Cow<str>, std::borrow::Cow<str>>>,
    ) -> ResponseFuture {
        debug!(
            "Received request with following headers {:?}",
            req.headers()
        );

        let range = req.headers().typed_get::<Range>();

        let bytes_range = match range.map(|r| r.iter().collect::<Vec<_>>()) {
            Some(bytes_ranges) => {
                if bytes_ranges.is_empty() {
                    error!("Range without data");
                    return short_response_boxed(StatusCode::BAD_REQUEST, "One range is required");
                } else if bytes_ranges.len() > 1 {
                    error!("Range with multiple ranges is not supported");
                    return short_response_boxed(
                        StatusCode::NOT_IMPLEMENTED,
                        "Do not support muptiple ranges",
                    );
                } else {
                    Some(bytes_ranges[0])
                }
            }

            None => None,
        };
        let seek: Option<f32> = params
            .as_mut()
            .and_then(|p| p.remove("seek"))
            .and_then(|s| s.parse().ok());
        let transcoding_quality: Option<QualityLevel> = params
            .and_then(|mut p| p.remove("trans"))
            .and_then(|t| QualityLevel::from_letter(&t));

        send_file(
            base_dir,
            get_subpath(&path, "/audio/"),
            bytes_range,
            seek,
            transcoding,
            transcoding_quality,
        )
    }
}

lazy_static! {
    static ref COLLECTION_NUMBER_RE: Regex = Regex::new(r"^/(\d+)/.+").unwrap();
}

fn extract_collection_number(path: String) -> Result<(String, usize), ()> {
    let matches = COLLECTION_NUMBER_RE.captures(&path);
    if let Some(matches) = matches {
        let cnum = matches.get(1).unwrap();
        // match gives us char position it's safe to slice
        let new_path = (&path[cnum.end()..]).to_string();
        // and cnum is guarateed to contain digits only
        let cnum: usize = cnum.as_str().parse().unwrap();
        if cnum >= get_config().base_dirs.len() {
            return Err(());
        }
        Ok((new_path, cnum))
    } else {
        Ok((path, 0))
    }
}
