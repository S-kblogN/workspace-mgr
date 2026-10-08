//! Continue an interrupted response read on its existing HTTP connection.
//! This never reconnects, rewinds an upload body, or resends request bytes.

use std::io::{self, ErrorKind, Read};
use std::time::Instant;

use ureq::Error;
use ureq::unversioned::transport::time::Duration as TransportDuration;
use ureq::unversioned::transport::{Buffers, ConnectionDetails, Connector, NextTimeout, Transport};

/// Continue a source-file read at its current position after a signal.
/// No seeking, body buffering, or HTTP request retry occurs here.
#[derive(Debug)]
pub(crate) struct ReadResumingReader<R> {
    delegate: R,
}

impl<R> ReadResumingReader<R> {
    pub(crate) fn new(delegate: R) -> Self {
        Self { delegate }
    }
}

impl<R: Read> Read for ReadResumingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.delegate.read(buffer) {
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct ReadResumingConnector<C> {
    delegate: C,
}

impl<C> ReadResumingConnector<C> {
    pub(crate) fn new(delegate: C) -> Self {
        Self { delegate }
    }
}

impl<In, C> Connector<In> for ReadResumingConnector<C>
where
    In: Transport,
    C: Connector<In>,
{
    type Out = ReadResumingTransport<C::Out>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, Error> {
        self.delegate
            .connect(details, chained)
            .map(|transport| transport.map(|delegate| ReadResumingTransport { delegate }))
    }
}

#[derive(Debug)]
pub(crate) struct ReadResumingTransport<T> {
    delegate: T,
}

impl<T: Transport> Transport for ReadResumingTransport<T> {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.delegate.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        // Transport exposes no partial write offset. Retrying this operation
        // could duplicate request bytes, even on the same connection.
        self.delegate.transmit_output(amount, timeout)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        if matches!(timeout.after, TransportDuration::Exact(budget) if budget.is_zero()) {
            return Err(Error::Timeout(timeout.reason));
        }
        let started = Instant::now();
        let mut remaining = timeout;
        loop {
            match self.delegate.await_input(remaining) {
                Err(Error::Io(error)) if error.kind() == ErrorKind::Interrupted => {
                    if let TransportDuration::Exact(budget) = timeout.after {
                        let Some(after) = budget
                            .checked_sub(started.elapsed())
                            .filter(|after| !after.is_zero())
                        else {
                            return Err(Error::Timeout(timeout.reason));
                        };
                        // Keep the original deadline and timeout reason. A
                        // signal must not grant another complete timeout budget.
                        remaining.after = TransportDuration::Exact(after);
                    }
                }
                result => return result,
            }
        }
    }

    fn is_open(&mut self) -> bool {
        self.delegate.is_open()
    }

    fn is_tls(&self) -> bool {
        self.delegate.is_tls()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::Duration;
    use ureq::Timeout;
    use ureq::unversioned::transport::LazyBuffers;

    #[derive(Debug)]
    enum ReadOutcome {
        Io(ErrorKind),
        InterruptedAfter(Duration),
        Timeout(Timeout),
        Bytes(&'static [u8]),
        Eof,
    }

    #[derive(Debug)]
    struct StubTransport {
        buffers: LazyBuffers,
        reads: VecDeque<ReadOutcome>,
        read_timeouts: Vec<NextTimeout>,
        input_appends: usize,
        writes: Vec<(usize, NextTimeout, Vec<u8>)>,
        write_error: Option<ErrorKind>,
        open_checks: usize,
    }

    impl StubTransport {
        fn new(reads: impl IntoIterator<Item = ReadOutcome>) -> Self {
            Self {
                buffers: LazyBuffers::new(128, 128),
                reads: reads.into_iter().collect(),
                read_timeouts: Vec::new(),
                input_appends: 0,
                writes: Vec::new(),
                write_error: None,
                open_checks: 0,
            }
        }
    }

    impl Transport for StubTransport {
        fn buffers(&mut self) -> &mut dyn Buffers {
            &mut self.buffers
        }

        fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
            self.writes
                .push((amount, timeout, self.buffers.output()[..amount].to_vec()));
            if let Some(kind) = self.write_error {
                Err(Error::Io(io::Error::from(kind)))
            } else {
                Ok(())
            }
        }

        fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
            self.read_timeouts.push(timeout);
            match self.reads.pop_front().expect("unexpected additional read") {
                ReadOutcome::Io(kind) => Err(Error::Io(io::Error::from(kind))),
                ReadOutcome::InterruptedAfter(delay) => {
                    std::thread::sleep(delay);
                    Err(Error::Io(io::Error::from(ErrorKind::Interrupted)))
                }
                ReadOutcome::Timeout(reason) => Err(Error::Timeout(reason)),
                ReadOutcome::Bytes(bytes) => {
                    self.buffers.input_append_buf()[..bytes.len()].copy_from_slice(bytes);
                    self.buffers.input_appended(bytes.len());
                    self.input_appends += 1;
                    Ok(true)
                }
                ReadOutcome::Eof => Ok(false),
            }
        }

        fn is_open(&mut self) -> bool {
            self.open_checks += 1;
            false
        }

        fn is_tls(&self) -> bool {
            true
        }
    }

    fn timeout() -> NextTimeout {
        NextTimeout {
            after: TransportDuration::Exact(Duration::from_secs(60)),
            reason: Timeout::RecvResponse,
        }
    }

    #[test]
    fn interrupted_reads_resume_existing_buffers_without_transmitting_again() {
        let mut transport = ReadResumingTransport {
            delegate: StubTransport::new([
                ReadOutcome::Io(ErrorKind::Interrupted),
                ReadOutcome::Io(ErrorKind::Interrupted),
                ReadOutcome::Bytes(b"suffix"),
            ]),
        };
        transport.buffers().input_append_buf()[..6].copy_from_slice(b"prefix");
        transport.buffers().input_appended(6);
        assert!(transport.await_input(timeout()).unwrap());
        assert_eq!(transport.buffers().input(), b"prefixsuffix");
        assert_eq!(transport.delegate.input_appends, 1);
        assert_eq!(transport.delegate.read_timeouts.len(), 3);
        assert!(transport.delegate.writes.is_empty());
    }

    #[test]
    fn interruptions_reduce_the_original_deadline_and_preserve_its_reason() {
        let initial = timeout();
        let mut transport = ReadResumingTransport {
            delegate: StubTransport::new([
                ReadOutcome::Io(ErrorKind::Interrupted),
                ReadOutcome::Io(ErrorKind::Interrupted),
                ReadOutcome::Bytes(b"ok"),
            ]),
        };
        assert!(transport.await_input(initial).unwrap());
        let observed = &transport.delegate.read_timeouts;
        assert_eq!(observed[0], initial);
        assert!(observed[2].after < observed[0].after);
        assert!(observed[2].after <= observed[1].after);
        assert!(observed.iter().all(|value| value.reason == initial.reason));

        let mut expired = ReadResumingTransport {
            delegate: StubTransport::new([ReadOutcome::Io(ErrorKind::Interrupted)]),
        };
        let error = expired
            .await_input(NextTimeout {
                after: TransportDuration::Exact(Duration::ZERO),
                reason: Timeout::Global,
            })
            .unwrap_err();
        assert!(matches!(error, Error::Timeout(Timeout::Global)));
        assert!(expired.delegate.read_timeouts.is_empty());

        let mut expired = ReadResumingTransport {
            delegate: StubTransport::new([ReadOutcome::InterruptedAfter(Duration::from_millis(2))]),
        };
        let error = expired
            .await_input(NextTimeout {
                after: TransportDuration::Exact(Duration::from_millis(1)),
                reason: Timeout::RecvResponse,
            })
            .unwrap_err();
        assert!(matches!(error, Error::Timeout(Timeout::RecvResponse)));
        assert_eq!(expired.delegate.read_timeouts.len(), 1);
    }

    #[test]
    fn disabled_timeout_allows_same_connection_read_continuation() {
        let mut outcomes = (0..4)
            .map(|_| ReadOutcome::Io(ErrorKind::Interrupted))
            .collect::<Vec<_>>();
        outcomes.push(ReadOutcome::Bytes(b"ok"));
        let mut transport = ReadResumingTransport {
            delegate: StubTransport::new(outcomes),
        };
        let unlimited = NextTimeout {
            after: TransportDuration::NotHappening,
            reason: Timeout::Global,
        };
        assert!(transport.await_input(unlimited).unwrap());
        assert_eq!(transport.delegate.read_timeouts, vec![unlimited; 5]);
    }

    #[test]
    fn other_read_errors_and_end_of_stream_are_forwarded_without_retry() {
        for kind in [
            ErrorKind::ConnectionReset,
            ErrorKind::PermissionDenied,
            ErrorKind::TimedOut,
            ErrorKind::WouldBlock,
            ErrorKind::UnexpectedEof,
        ] {
            let mut transport = ReadResumingTransport {
                delegate: StubTransport::new([ReadOutcome::Io(kind)]),
            };
            let error = transport.await_input(timeout()).unwrap_err();
            assert!(matches!(error, Error::Io(error) if error.kind() == kind));
            assert_eq!(transport.delegate.read_timeouts.len(), 1);
        }
        let mut transport = ReadResumingTransport {
            delegate: StubTransport::new([ReadOutcome::Timeout(Timeout::RecvBody)]),
        };
        assert!(matches!(
            transport.await_input(timeout()).unwrap_err(),
            Error::Timeout(Timeout::RecvBody)
        ));
        assert_eq!(transport.delegate.read_timeouts.len(), 1);
        let mut transport = ReadResumingTransport {
            delegate: StubTransport::new([ReadOutcome::Eof]),
        };
        assert!(!transport.await_input(timeout()).unwrap());
        assert_eq!(transport.delegate.read_timeouts.len(), 1);
    }

    #[test]
    fn interrupted_transmission_is_not_retried_and_connection_properties_are_forwarded() {
        let mut delegate = StubTransport::new([]);
        delegate.write_error = Some(ErrorKind::Interrupted);
        let mut transport = ReadResumingTransport { delegate };
        transport.buffers().output()[..4].copy_from_slice(b"once");
        let initial = timeout();
        let error = transport.transmit_output(4, initial).unwrap_err();
        assert!(matches!(error, Error::Io(error) if error.kind() == ErrorKind::Interrupted));
        assert_eq!(transport.delegate.writes, [(4, initial, b"once".to_vec())]);
        assert!(transport.delegate.read_timeouts.is_empty());
        assert!(!transport.is_open());
        assert_eq!(transport.delegate.open_checks, 1);
        assert!(transport.is_tls());
    }

    #[derive(Debug)]
    struct ScriptedReader {
        bytes: &'static [u8],
        position: usize,
        steps: VecDeque<Result<usize, ErrorKind>>,
        calls: usize,
    }

    impl ScriptedReader {
        fn new(steps: impl IntoIterator<Item = Result<usize, ErrorKind>>) -> Self {
            Self {
                bytes: b"abcdef",
                position: 0,
                steps: steps.into_iter().collect(),
                calls: 0,
            }
        }
    }

    impl Read for ScriptedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if buffer.is_empty() {
                return Ok(0);
            }
            self.calls += 1;
            let size = self
                .steps
                .pop_front()
                .expect("unexpected additional source read")
                .map_err(io::Error::from)?
                .min(buffer.len())
                .min(self.bytes.len() - self.position);
            buffer[..size].copy_from_slice(&self.bytes[self.position..self.position + size]);
            self.position += size;
            Ok(size)
        }
    }

    #[test]
    fn interrupted_source_reads_preserve_the_current_byte_position() {
        let mut reader = ReadResumingReader::new(ScriptedReader::new([
            Err(ErrorKind::Interrupted),
            Ok(2),
            Err(ErrorKind::Interrupted),
            Ok(2),
            Ok(2),
            Ok(0),
        ]));
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 4];
        for expected in [2, 2, 2, 0] {
            let amount = reader.read(&mut buffer).unwrap();
            assert_eq!(amount, expected);
            bytes.extend_from_slice(&buffer[..amount]);
        }
        assert_eq!(bytes, b"abcdef");
        assert_eq!(reader.delegate.position, 6);
        assert_eq!(reader.delegate.calls, 6);
    }

    #[test]
    fn interrupted_limited_source_reads_neither_reset_nor_exceed_the_limit() {
        let source = ScriptedReader::new([
            Err(ErrorKind::Interrupted),
            Ok(2),
            Err(ErrorKind::Interrupted),
            Ok(9),
        ]);
        let mut reader = ReadResumingReader::new(source.take(4));
        let mut buffer = [0u8; 8];
        assert_eq!(reader.read(&mut buffer).unwrap(), 2);
        assert_eq!(&buffer[..2], b"ab");
        assert_eq!(reader.delegate.limit(), 2);
        assert_eq!(reader.read(&mut buffer).unwrap(), 2);
        assert_eq!(&buffer[..2], b"cd");
        assert_eq!(reader.delegate.limit(), 0);
        assert_eq!(reader.read(&mut buffer).unwrap(), 0);
        assert_eq!(reader.delegate.get_ref().position, 4);
        assert_eq!(reader.delegate.get_ref().calls, 4);
    }

    #[test]
    fn other_source_read_errors_are_returned_without_advancing_or_retrying() {
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::UnexpectedEof,
            ErrorKind::TimedOut,
            ErrorKind::ConnectionReset,
        ] {
            let mut reader = ReadResumingReader::new(ScriptedReader::new([Err(kind)]));
            assert_eq!(reader.read(&mut [0u8; 8]).unwrap_err().kind(), kind);
            assert_eq!(reader.delegate.position, 0);
            assert_eq!(reader.delegate.calls, 1);
        }
    }
}
