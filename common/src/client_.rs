// Copyright 2017 rust-multipart-rfc7578 Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.
//

use crate::error::Error;
use bytes::{
    buf::{BufMutExt, IoSliceMut},
    BufMut, Bytes, BytesMut,
};
use futures::{stream::Stream, task::{Context, Poll}};
use http::{
    self,
    header::{self, HeaderName},
    request::{Builder, Request},
};
use mime::{self, Mime};
use rand::{distributions::Alphanumeric, rngs::SmallRng, FromEntropy, Rng};
use std::{
    borrow::Borrow,
    fmt::Display,
    fs::File,
    io::{self, Cursor, Read, Write},
    iter::{FromIterator, Peekable},
    path::Path,
    pin::Pin,
    str::FromStr,
    vec::IntoIter,
};

static CONTENT_DISPOSITION: HeaderName = header::CONTENT_DISPOSITION;
static CONTENT_TYPE: HeaderName = header::CONTENT_TYPE;
static CONTENT_LENGTH: HeaderName = header::CONTENT_LENGTH;

/// Writes a CLRF.
///
fn write_crlf<W>(write: &mut W) -> io::Result<()>
where
    W: Write,
{
    write.write_all(&[b'\r', b'\n'])
}

/// Async streamable Multipart body.
///
pub struct Body<'a> {
    /// The amount of data to write with each chunk.
    ///
    buf_size: usize,

    /// The active reader.
    ///
    current: Option<Box<dyn 'a + Read + Send + Sync>>,

    /// The parts as an iterator. When the iterator stops
    /// yielding, the body is fully written.
    ///
    parts: Peekable<IntoIter<Part<'a>>>,

    /// The multipart boundary.
    ///
    boundary: String,
}

impl<'a> Body<'a> {
    /// Implements section 4.1.
    ///
    /// [See](https://tools.ietf.org/html/rfc7578#section-4.1).
    ///
    fn write_boundary<W>(&self, write: &mut W) -> io::Result<()>
    where
        W: Write,
    {
        write_crlf(write)?;
        write.write_all(&[b'-', b'-'])?;
        write.write_all(self.boundary.as_bytes())
    }

    /// Writes the last form boundary.
    ///
    /// [See](https://tools.ietf.org/html/rfc2046#section-5.1).
    ///
    fn write_final_boundary<W>(&self, write: &mut W) -> io::Result<()>
    where
        W: Write,
    {
        self.write_boundary(write)?;
        write.write_all(&[b'-', b'-'])
    }

    /// Writes the Content-Disposition, and Content-Type headers.
    ///
    fn write_headers<W>(&self, write: &mut W, part: &Part) -> io::Result<()>
    where
        W: Write,
    {
        write_crlf(write)?;
        write.write_all(CONTENT_TYPE.as_ref())?;
        write.write_all(b": ")?;
        write.write_all(part.content_type.as_bytes())?;
        write_crlf(write)?;
        write.write_all(CONTENT_DISPOSITION.as_ref())?;
        write.write_all(b": ")?;
        write.write_all(part.content_disposition.as_bytes())?;
        write_crlf(write)?;
        if let Some(content_length) = part.content_length() {
            write.write_all(CONTENT_LENGTH.as_ref())?;
            write.write_all(b": ")?;
            write!(write, "{}", content_length)?;
        }
        write_crlf(write)?;
        write_crlf(write)
    }
}

impl<'a> Stream for Body<'a> {
    type Item = Result<Bytes, Error>;

    /// Iterate over each form part, and write it out.
    ///
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let bytes = BytesMut::with_capacity(self.buf_size);
        let mut writer = bytes.writer();

        if self.current.is_none() {
            if let Some(part) = self.parts.next() {
                self.write_boundary(&mut writer)
                    .map_err(Error::BoundaryWrite)?;
                self.write_headers(&mut writer, &part)
                    .map_err(Error::HeaderWrite)?;

                let read = match part.inner {
                    Inner::Read(read, _) => read,
                    Inner::Text(s) => Box::new(Cursor::new(s.into_bytes())),
                };

                self.current = Some(read);
            } else {
                // No current part, and no parts left means there is nothing
                // left to write.
                //
                return Poll::Ready(None);
            }
        }

        let num = if let Some(ref mut read) = self.current {
            let slice = IoSliceMut::from(writer.get_mut().bytes_mut());
            unsafe {
                let mut slice: std::io::IoSliceMut = std::mem::transmute(slice);
                let num = read.read(&mut slice).map_err(Error::ContentRead)?;

                writer.get_mut().advance_mut(num);

                num
            }
        } else {
            0
        };

        if num == 0 {
            // Wrote 0 bytes from the reader, so we reached the EOF for the
            // current item.
            //
            self.current = None;

            // Peek to check if there are are any parts not yet written.
            // If there is nothing, the final boundary can be written.
            //
            if self.parts.peek().is_none() {
                self.write_final_boundary(&mut writer)
                    .map_err(Error::BoundaryWrite)?;

                Poll::Ready(Some(Ok(writer.into_inner().freeze())))
            } else {
                self.poll_next(cx)
            }
        } else {
            Poll::Ready(Some(Ok(writer.into_inner().freeze())))
        }
    }
}

/// Implements the multipart/form-data media type as described by
/// RFC 7578.
///
/// [See](https://tools.ietf.org/html/rfc7578#section-1).
///
pub struct Form<'a> {
    parts: Vec<Part<'a>>,

    /// The auto-generated boundary as described by 4.1.
    ///
    /// [See](https://tools.ietf.org/html/rfc7578#section-4.1).
    ///
    boundary: String,
}

impl<'a> Default for Form<'a> {
    /// Creates a new form with the default boundary generator.
    ///
    #[inline]
    fn default() -> Form<'a> {
        Form::new::<RandomAsciiGenerator>()
    }
}

impl<'a> Form<'a> {
    /// Creates a new form with the specified boundary generator function.
    ///
    /// # Examples
    ///
    /// ```
    /// # use common_multipart_rfc7578::client::multipart::{
    /// #     self,
    /// #     BoundaryGenerator
    /// # };
    /// #
    /// struct TestGenerator;
    ///
    /// impl BoundaryGenerator for TestGenerator {
    ///     fn generate_boundary() -> String {
    ///         "test".to_string()
    ///     }
    /// }
    ///
    /// let form = multipart::Form::new::<TestGenerator>();
    /// ```
    ///
    #[inline]
    pub fn new<G>() -> Form<'a>
    where
        G: BoundaryGenerator,
    {
        Form {
            parts: vec![],
            boundary: G::generate_boundary(),
        }
    }

    /// Adds a text part to the Form.
    ///
    /// # Examples
    ///
    /// ```
    /// use common_multipart_rfc7578::client::multipart;
    ///
    /// let mut form = multipart::Form::default();
    ///
    /// form.add_text("text", "Hello World!");
    /// form.add_text("more", String::from("Hello Universe!"));
    /// ```
    ///
    pub fn add_text<N, T>(&mut self, name: N, text: T)
    where
        N: Display,
        T: Into<String>,
    {
        self.parts.push(Part::new::<_, String>(
            Inner::Text(text.into()),
            name,
            None,
            None,
        ))
    }

    /// Adds a readable part to the Form.
    ///
    /// # Examples
    ///
    /// ```
    /// use common_multipart_rfc7578::client::multipart;
    /// use std::io::Cursor;
    ///
    /// let bytes = Cursor::new("Hello World!");
    /// let mut form = multipart::Form::default();
    ///
    /// form.add_reader("input", bytes);
    /// ```
    ///
    pub fn add_reader<F, R>(&mut self, name: F, read: R)
    where
        F: Display,
        R: 'a + Read + Send + Sync,
    {
        let read = Box::new(read);

        self.parts.push(Part::new::<_, String>(
            Inner::Read(read, None),
            name,
            None,
            None,
        ));
    }

    /// Adds a file, and attempts to derive the mime type.
    ///
    /// # Examples
    ///
    /// ```
    /// use common_multipart_rfc7578::client::multipart;
    ///
    /// let mut form = multipart::Form::default();
    ///
    /// form.add_file("file", file!()).expect("file to exist");
    /// ```
    ///
    #[inline]
    pub fn add_file<P, F>(&mut self, name: F, path: P) -> io::Result<()>
    where
        P: AsRef<Path>,
        F: Display,
    {
        self._add_file(name, path, None)
    }

    /// Adds a readable part to the Form as a file.
    ///
    /// # Examples
    ///
    /// ```
    /// use common_multipart_rfc7578::client::multipart;
    /// use std::io::Cursor;
    ///
    /// let bytes = Cursor::new("Hello World!");
    /// let mut form = multipart::Form::default();
    ///
    /// form.add_reader_file("input", bytes, "filename.txt");
    /// ```
    ///
    pub fn add_reader_file<F, G, R>(&mut self, name: F, read: R, filename: G)
    where
        F: Display,
        G: Into<String>,
        R: 'a + Read + Send + Sync,
    {
        let read = Box::new(read);

        self.parts.push(Part::new::<_, String>(
            Inner::Read(read, None),
            name,
            None,
            Some(filename.into()),
        ));
    }

    /// Adds a readable part to the Form as a file with a specified mime.
    ///
    /// # Examples
    ///
    /// ```
    /// use common_multipart_rfc7578::client::multipart;
    /// use std::io::Cursor;
    ///
    /// let bytes = Cursor::new("Hello World!");
    /// let mut form = multipart::Form::default();
    ///
    /// form.add_reader_file_with_mime("input", bytes, "filename.txt", mime::TEXT_PLAIN);
    /// ```
    ///
    pub fn add_reader_file_with_mime<F, G, R>(&mut self, name: F, read: R, filename: G, mime: Mime)
    where
        F: Display,
        G: Into<String>,
        R: 'a + Read + Send + Sync,
    {
        let read = Box::new(read);

        self.parts.push(Part::new::<_, String>(
            Inner::Read(read, None),
            name,
            Some(mime),
            Some(filename.into()),
        ));
    }

    /// Adds a file with the specified mime type to the form.
    /// If the mime type isn't specified, a mime type will try to
    /// be derived.
    ///
    /// # Examples
    ///
    /// ```
    /// use common_multipart_rfc7578::client::multipart;
    ///
    /// let mut form = multipart::Form::default();
    ///
    /// form.add_file_with_mime("data", "test.csv", mime::TEXT_CSV);
    /// ```
    ///
    #[inline]
    pub fn add_file_with_mime<P, F>(&mut self, name: F, path: P, mime: Mime) -> io::Result<()>
    where
        P: AsRef<Path>,
        F: Display,
    {
        self._add_file(name, path, Some(mime))
    }

    /// Internal method for adding a file part to the form.
    ///
    fn _add_file<P, F>(&mut self, name: F, path: P, mime: Option<Mime>) -> io::Result<()>
    where
        P: AsRef<Path>,
        F: Display,
    {
        let f = File::open(&path)?;
        let mime = if let Some(ext) = path.as_ref().extension() {
            Mime::from_str(ext.to_string_lossy().borrow()).ok()
        } else {
            mime
        };
        let len = match f.metadata() {
            // If the path is not a file, it can't be uploaded because there
            // is no content.
            //
            Ok(ref meta) if !meta.is_file() => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a file not directory",
            )),

            // If there is some metadata on the file, try to derive some
            // header values.
            //
            Ok(ref meta) => Ok(Some(meta.len())),

            // The file metadata could not be accessed. This MIGHT not be an
            // error, if the file could be opened.
            //
            Err(e) => Err(e),
        }?;

        let read = Box::new(f);

        self.parts.push(Part::new(
            Inner::Read(read, len),
            name,
            mime,
            Some(path.as_ref().as_os_str().to_string_lossy()),
        ));

        Ok(())
    }

    /// Updates a request instance with the multipart Content-Type header
    /// and the payload data.
    ///
    /// # Examples
    ///
    /// ```
    /// use hyper::{Method, Request};
    /// use hyper_multipart_rfc7578::client::multipart;
    ///
    /// let mut req_builder = Request::post("http://localhost:80/upload");
    /// let mut form = multipart::Form::default();
    ///
    /// form.add_text("text", "Hello World!");
    /// let req = form.set_body::<multipart::Body>(req_builder).unwrap();
    /// ```
    ///
    #[inline]
    pub fn set_body<B>(self, req: Builder) -> Result<Request<B>, http::Error>
    where
        B: From<Body<'a>>,
    {
        self.set_body_convert::<B, B>(req)
    }

    /// Updates a request instance with the multipart Content-Type header
    /// and the payload data.
    ///
    /// Allows converting body into an intermediate type.
    ///
    /// # Examples
    ///
    /// ```
    /// use hyper::{Body, Method, Request};
    /// use hyper_multipart_rfc7578::client::multipart;
    ///
    /// let mut req_builder = Request::post("http://localhost:80/upload");
    /// let mut form = multipart::Form::default();
    ///
    /// form.add_text("text", "Hello World!");
    /// let req = form.set_body_convert::<hyper::Body, multipart::Body>(req_builder).unwrap();
    /// ```
    ///
    pub fn set_body_convert<B, I>(self, req: Builder) -> Result<Request<B>, http::Error>
    where
        I: From<Body<'a>> + Into<B>,
    {
        req.header(&CONTENT_TYPE, self.content_type().as_str())
            .body(I::from(Body::from(self)).into())
    }

    pub fn content_type(&self) -> String {
        format!("multipart/form-data; boundary=\"{}\"", &self.boundary)
    }
}

impl<'a> From<Form<'a>> for Body<'a> {
    /// Turns a `Form` into a multipart `Body`.
    ///
    #[inline]
    fn from(form: Form<'a>) -> Self {
        Body {
            buf_size: 2048,
            current: None,
            parts: form.parts.into_iter().peekable(),
            boundary: form.boundary,
        }
    }
}

/// One part of a body delimited by a boundary line.
///
/// [See RFC2046 5.1](https://tools.ietf.org/html/rfc2046#section-5.1).
///
pub struct Part<'a> {
    inner: Inner<'a>,

    /// Each part can include a Content-Type header field. If this
    /// is not specified, it defaults to "text/plain", or
    /// "application/octet-stream" for file data.
    ///
    /// [See](https://tools.ietf.org/html/rfc7578#section-4.4)
    ///
    content_type: String,

    /// Each part must contain a Content-Disposition header field.
    ///
    /// [See](https://tools.ietf.org/html/rfc7578#section-4.2).
    ///
    content_disposition: String,
}

impl<'a> Part<'a> {
    /// Internal method to build a new Part instance. Sets the disposition type,
    /// content-type, and the disposition parameters for name, and optionally
    /// for filename.
    ///
    /// Per [4.3](https://tools.ietf.org/html/rfc7578#section-4.3), if multiple
    /// files need to be specified for one form field, they can all be specified
    /// with the same name parameter.
    ///
    fn new<N, F>(inner: Inner<'a>, name: N, mime: Option<Mime>, filename: Option<F>) -> Part<'a>
    where
        N: Display,
        F: Display,
    {
        // `name` disposition parameter is required. It should correspond to the
        // name of a form field.
        //
        // [See 4.2](https://tools.ietf.org/html/rfc7578#section-4.2)
        //
        let mut disposition_params = vec![format!("name=\"{}\"", name)];

        // `filename` can be supplied for files, but is totally optional.
        //
        // [See 4.2](https://tools.ietf.org/html/rfc7578#section-4.2)
        //
        if let Some(filename) = filename {
            disposition_params.push(format!("filename=\"{}\"", filename));
        }

        let content_type = format!("{}", mime.unwrap_or_else(|| inner.default_content_type()));

        Part {
            inner,
            content_type,
            content_disposition: format!("form-data; {}", disposition_params.join("; ")),
        }
    }

    fn content_length(&self) -> Option<u64> {
        match &self.inner {
            Inner::Read(_, s) => s.clone(),
            Inner::Text(t) => Some(t.len() as u64),
        }
    }
}

enum Inner<'a> {
    /// The `Read` variant captures multiple cases.
    ///
    ///   * The first is it supports uploading a file, which is explicitly
    ///     described in RFC 7578.
    ///
    ///   * The second (which is not described by RFC 7578), is it can handle
    ///     arbitrary input streams (for example, a server response).
    ///     Any arbitrary input stream is automatically considered a file,
    ///     and assigned the corresponding content type if not explicitly
    ///     specified.
    ///
    Read(Box<dyn 'a + Read + Send + Sync>, Option<u64>),

    /// The `String` variant handles "text/plain" form data payloads.
    ///
    Text(String),
}

impl<'a> Inner<'a> {
    /// Returns the default Content-Type header value as described in section 4.4.
    ///
    /// [See](https://tools.ietf.org/html/rfc7578#section-4.4)
    ///
    #[inline]
    fn default_content_type(&self) -> Mime {
        match *self {
            Inner::Read(_, _) => mime::APPLICATION_OCTET_STREAM,
            Inner::Text(_) => mime::TEXT_PLAIN,
        }
    }
}

/// A `BoundaryGenerator` is a policy to generate a random string to use
/// as a part boundary.
///
/// The default generator will build a random string of 6 ascii characters.
/// If you need more complexity, you can implement this, and use it with
/// [`Form::new`].
///
/// # Examples
///
/// ```
/// use common_multipart_rfc7578::client::multipart::BoundaryGenerator;
///
/// struct TestGenerator;
///
/// impl BoundaryGenerator for TestGenerator {
///     fn generate_boundary() -> String {
///         "test".to_string()
///     }
/// }
/// ```
pub trait BoundaryGenerator {
    /// Generates a String to use as a boundary.
    ///
    fn generate_boundary() -> String;
}

struct RandomAsciiGenerator;

impl BoundaryGenerator for RandomAsciiGenerator {
    /// Creates a boundary of 6 ascii characters.
    ///
    fn generate_boundary() -> String {
        let mut rng = SmallRng::from_entropy();
        let ascii = rng.sample_iter(&Alphanumeric);

        String::from_iter(ascii.take(6))
    }
}
