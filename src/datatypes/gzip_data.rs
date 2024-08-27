use std::io;

use rocket::data::{Data, FromData, Limits, Outcome};
use rocket::http::Status;
use rocket::request::{local_cache, Request};

use libflate::gzip::Decoder;
use rocket::serde::json::Error;
use rocket::serde::Deserialize;
use std::io::Read;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LoggedBodyJSON<T> {
    pub body: T,
    pub size: usize,
}

impl<T> LoggedBodyJSON<T> {
    #[inline(always)]
    pub fn into_inner(self) -> T {
        self.body
    }
}

impl<'r, T: Deserialize<'r>> LoggedBodyJSON<T> {
    async fn from_data(req: &'r Request<'_>, data: Data<'r>) -> Result<Self, Error<'r>> {
        let is_gzipped = req.headers().get_one("Content-Encoding") == Some("gzip");
        let limit = req.limits().get("string").unwrap_or(Limits::JSON);

        let raw_bytes = match data.open(limit).into_bytes().await {
            Ok(s) if s.is_complete() => s.into_inner(),
            Ok(_) => {
                println!("Error -- Request body too large");

                let eof = io::ErrorKind::UnexpectedEof;
                return Err(Error::Io(io::Error::new(eof, "data limit exceeded")));
            }
            Err(e) => {
                return Err(Error::Io(e));
            }
        };

        let maybe_s = match is_gzipped {
            true => {
                let decoder_result = Decoder::new(&raw_bytes[..]);
                if decoder_result.is_err() {
                    return Err(Error::Io(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "cant decode gzip data",
                    )));
                }
                let mut decoder = decoder_result.unwrap();

                let mut buf = Vec::new();
                let mut chunk = [0; 4096];
                let mut total_size = 0;

                loop {
                    let bytes_read_result = decoder.read(&mut chunk);
                    if bytes_read_result.is_err() {
                        return Err(Error::Io(bytes_read_result.err().unwrap()));
                    }

                    let bytes_read = bytes_read_result.unwrap();
                    if bytes_read == 0 {
                        break;
                    }
                    total_size += bytes_read;
                    if total_size > limit {
                        return Err(Error::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "data limit exceeded",
                        )));
                    }
                    buf.extend_from_slice(&chunk[..bytes_read]);
                }
                String::from_utf8(buf)
            }
            false => String::from_utf8(raw_bytes),
        };

        let s = match maybe_s {
            Ok(s) => s,
            Err(_e) => {
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Non-utf8 string",
                )));
            }
        };

        let s_with_lifetime = local_cache!(req, s);
        rocket::serde::json::from_str(s_with_lifetime)
            .map(|json| -> LoggedBodyJSON<T> {
                LoggedBodyJSON {
                    body: json,
                    size: s_with_lifetime.len(),
                }
            })
            .map_err(|e| {
                println!("Error -- Malformed JSON");
                Error::Parse(s_with_lifetime, e)
            })
    }
}

#[rocket::async_trait]
impl<'r, T: Deserialize<'r>> FromData<'r> for LoggedBodyJSON<T> {
    type Error = Error<'r>;

    async fn from_data(req: &'r Request<'_>, data: Data<'r>) -> Outcome<'r, Self> {
        match Self::from_data(req, data).await {
            Ok(value) => Outcome::Success(value),
            Err(Error::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                Outcome::Error((Status::PayloadTooLarge, Error::Io(e)))
            }
            Err(Error::Parse(s, e)) if e.classify() == serde_json::error::Category::Data => {
                Outcome::Error((Status::UnprocessableEntity, Error::Parse(s, e)))
            }
            Err(e) => Outcome::Error((Status::BadRequest, e)),
        }
    }
}
