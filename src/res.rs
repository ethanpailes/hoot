use core::marker::PhantomData;
use core::mem;
use core::str;

use httparse::Header;

use crate::parser::{parse_headers, parse_response_line};
use crate::req::CallState;
use crate::util::compare_lowercase_ascii;
use crate::vars::private::*;
use crate::{state::*, HootError, HttpVersion};
use crate::{Result, ResumeToken};

pub struct Response<S: State> {
    _typ: PhantomData<S>,
    state: CallState,
}

impl Response<()> {
    pub fn resume(request: ResumeToken<ENDED, (), (), ()>) -> Response<RECV_RESPONSE> {
        Response {
            _typ: PhantomData,
            state: request.into_state(),
        }
    }
}

impl<S: State> Response<S> {
    fn transition<S2: State>(self) -> Response<S2> {
        // SAFETY: this only changes the type state of the PhantomData
        unsafe { mem::transmute(self) }
    }
}

pub struct ResponseAttempt<'a, 'b> {
    response: Response<RECV_RESPONSE>,
    success: bool,
    status: Option<Status<'a>>,
    headers: Option<&'b [Header<'a>]>,
}

impl<'a, 'b> ResponseAttempt<'a, 'b> {
    fn incomplete(response: Response<RECV_RESPONSE>) -> Self {
        ResponseAttempt {
            response,
            success: false,
            status: None,
            headers: None,
        }
    }

    pub fn is_success(&self) -> bool {
        self.success
    }

    pub fn status(&self) -> Option<&Status<'a>> {
        self.status.as_ref()
    }

    pub fn headers(&self) -> Option<&'b [Header<'a>]> {
        self.headers
    }

    pub fn next(self) -> AttemptNext {
        if !self.success {
            AttemptNext::Retry(self.response.transition())
        } else {
            match self.response.state.recv_body_mode.unwrap() {
                RecvBodyMode::LengthDelimited(v) if v == 0 => {
                    AttemptNext::NoBody(self.response.transition())
                }
                _ => AttemptNext::Body(self.response.transition()),
            }
        }
    }
}

pub enum AttemptNext {
    Retry(Response<RECV_RESPONSE>),
    Body(Response<RECV_BODY>),
    NoBody(Response<ENDED>),
}

impl AttemptNext {
    pub fn unwrap_retry(self) -> Response<RECV_RESPONSE> {
        let AttemptNext::Retry(r) = self else {
            panic!("unwrap_no_body when AttemptNext isnt Retry");
        };
        r
    }

    pub fn unwrap_body(self) -> Response<RECV_BODY> {
        let AttemptNext::Body(r) = self else {
            panic!("unwrap_no_body when AttemptNext isnt Body");
        };
        r
    }

    pub fn unwrap_no_body(self) -> Response<ENDED> {
        let AttemptNext::NoBody(r) = self else {
            panic!("unwrap_no_body when AttemptNext isnt NoBody");
        };
        r
    }
}

impl Response<RECV_RESPONSE> {
    pub fn try_read_response<'a, 'b>(
        mut self,
        input: &'a [u8],
        buf: &'b mut [u8],
    ) -> Result<ResponseAttempt<'a, 'b>> {
        let line = parse_response_line(input)?;
        if !line.complete {
            return Ok(ResponseAttempt::incomplete(self));
        }

        let status_offset = line.consumed;

        let headers = parse_headers(&input[status_offset..], buf)?;

        if !headers.complete {
            return Ok(ResponseAttempt::incomplete(self));
        }

        let is_http10 = self.state.is_head.unwrap();
        let is_head = self.state.is_head.unwrap();
        let mode = RecvBodyMode::from(is_http10, is_head, line.output.1, headers.output)?;
        self.state.recv_body_mode = Some(mode);

        Ok(ResponseAttempt {
            response: self,
            success: true,
            status: Some(line.output),
            headers: Some(headers.output),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Status<'a>(pub HttpVersion, pub u16, pub &'a str);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RecvBodyMode {
    /// Delimited by content-length. 0 is also a valid value when we don't expect a body,
    /// due to HEAD or status, but still want to leave the socket open.
    LengthDelimited(u64),
    /// Chunked transfer encoding
    Chunked,
    /// Expect remote to close at end of body.
    CloseDelimited,
}

impl RecvBodyMode {
    pub fn from(
        is_http10: bool,
        is_head: bool,
        status_code: u16,
        headers: &[Header<'_>],
    ) -> Result<Self> {
        let has_no_body =
            // https://datatracker.ietf.org/doc/html/rfc2616#section-4.3
            // All responses to the HEAD request method
            // MUST NOT include a message-body, even though the presence of entity-
            // header fields might lead one to believe they do.
            is_head ||
            // All 1xx (informational), 204 (no content), and 304 (not modified) responses
            // MUST NOT include a message-body.
            status_code >= 100 && status_code <= 199 ||
            matches!(status_code, 204 | 304);

        if has_no_body {
            return Ok(Self::LengthDelimited(0));
        }

        // https://datatracker.ietf.org/doc/html/rfc2616#section-4.3
        // All other responses do include a message-body, although it MAY be of zero length.

        let mut content_length: Option<u64> = None;
        let mut is_chunked = false;

        for head in headers {
            if compare_lowercase_ascii(head.name, "content-length") {
                let v = str::from_utf8(head.value)?.parse::<u64>()?;
                if content_length.is_some() {
                    return Err(HootError::DuplicateContentLength);
                }
                content_length = Some(v);
            } else if !is_chunked && compare_lowercase_ascii(head.name, "transfer-encoding") {
                // Header can repeat, stop looking if we found "chunked"
                let s = str::from_utf8(head.value)?;
                is_chunked = s
                    .split(",")
                    .map(|v| v.trim())
                    .any(|v| compare_lowercase_ascii(v, "chunked"));
            }
        }

        if is_chunked && !is_http10 {
            // https://datatracker.ietf.org/doc/html/rfc2616#section-4.4
            // Messages MUST NOT include both a Content-Length header field and a
            // non-identity transfer-coding. If the message does include a non-
            // identity transfer-coding, the Content-Length MUST be ignored.
            return Ok(Self::Chunked);
        }

        if let Some(len) = content_length {
            return Ok(Self::LengthDelimited(len));
        }

        Ok(Self::CloseDelimited)
    }
}

#[cfg(any(std, test))]
mod std_impls {
    use super::*;
    use std::fmt;

    impl fmt::Debug for Status<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_tuple("Status")
                .field(&self.0)
                .field(&self.1)
                .field(&self.2)
                .finish()
        }
    }
}
