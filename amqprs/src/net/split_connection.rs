use crate::frame::{Frame, FrameHeader};

use amqp_serde::{constants::FRAME_END, to_buffer, types::ShortUint};
use bytes::{Buf, BytesMut};
use serde::Serialize;
use std::io;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream,
    },
};

const DEFAULT_BUFFER_SIZE: usize = 8192;

pub struct SplitConnection;
pub struct Reader {
    stream: OwnedReadHalf,
    buffer: BytesMut,
}
pub struct Writer {
    stream: OwnedWriteHalf,
    buffer: BytesMut,
}

impl SplitConnection {
    pub async fn open(addr: &str) -> io::Result<(Reader, Writer)> {
        let stream = TcpStream::connect(addr).await?;
        let (reader, writer) = stream.into_split();

        let read_buffer = BytesMut::with_capacity(DEFAULT_BUFFER_SIZE);
        let write_buffer = BytesMut::with_capacity(DEFAULT_BUFFER_SIZE);

        Ok((
            Reader {
                stream: reader,
                buffer: read_buffer,
            },
            Writer {
                stream: writer,
                buffer: write_buffer,
            },
        ))
    }
}
impl Writer {
    pub async fn write<T: Serialize>(&mut self, value: &T) -> io::Result<usize> {
        to_buffer(value, &mut self.buffer)
            .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
        let len = self.buffer.len();
        self.stream.write_all(&self.buffer).await?;
        self.buffer.advance(len);
        Ok(len)
    }

    pub async fn write_frame(&mut self, channel: ShortUint, frame: Frame) -> io::Result<usize> {
        // reserve bytes for frame header, which to be updated after encoding payload
        let header = FrameHeader {
            frame_type: frame.get_frame_type(),
            channel,
            payload_size: 0,
        };
        to_buffer(&header, &mut self.buffer).unwrap();

        // encode payload
        let payload_size = to_buffer(&frame, &mut self.buffer)
            .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;

        // update frame's payload size
        for (i, v) in (payload_size as u32).to_be_bytes().iter().enumerate() {
            let p = self.buffer.get_mut(i + 3).unwrap();
            *p = *v;
        }

        // encode frame end byte
        to_buffer(&FRAME_END, &mut self.buffer).unwrap();

        // flush whole buffer
        self.stream.write_all(&self.buffer).await?;

        // discard sent data in write buffer
        let len = self.buffer.len();
        self.buffer.advance(len);

        Ok(len)
    }

    pub async fn close(&mut self) -> io::Result<()> {
        // TODO: flush buffers if is not empty?
        self.stream.shutdown().await
    }
}
impl Reader {
    /// To support channels multiplex on one connection
    /// we need to return the channel id.
    /// Return :
    ///     (channel_id, Frame)
    pub async fn read_frame(&mut self) -> io::Result<(ShortUint, Frame)> {
        // TODO: handle network error, such as timeout, corrupted frame
        loop {
            let len = self.stream.read_buf(&mut self.buffer).await?;
            if len == 0 {
                if self.buffer.is_empty() {
                    //TODO: map to own error
                    return Err(io::Error::new(io::ErrorKind::Other, "peer shutdown"));
                } else {
                    //TODO: map to own error
                    return Err(io::Error::new(io::ErrorKind::Other, "connection failure"));
                }
            }
            // TODO: replace with tracing
            println!("number of bytes read from network {len}");
            // println!("{:02X?}", self.buffer.as_ref());
            // println!("{:?}", self.buffer);

            match Frame::decode(&self.buffer) {
                Ok((len, channel, frame)) => {
                    // discard parsed data in read buffer
                    self.buffer.advance(len);
                    return Ok((channel, frame));
                }
                Err(err) => match err {
                    crate::frame::Error::Incomplete => continue,
                    crate::frame::Error::Corrupted => {
                        // TODO: map this error to indicate connection to be shutdown
                        return Err(io::Error::new(
                            io::ErrorKind::Other,
                            "corrupted frame, should close the connection",
                        ));
                    }
                    crate::frame::Error::Other(_) => todo!(),
                },
            }
        }
    }
}

#[cfg(test)]
mod test {

    use super::SplitConnection;
    use crate::frame::*;
    use tokio::runtime;

    fn new_runtime() -> runtime::Runtime {
        let rt = runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt
    }

    #[test]
    fn test_client_establish_connection() {
        // connection       = open-connection *use-connection close-connection
        // open-connection  = C:protocolheader
        //                 S:START C:STARTOK
        //                 *challenge
        //                 S:TUNE C:TUNEOK
        //                 C:OPEN S:OPENOK
        // challenge        = S:SECURE C:SECUREOK
        // use-connection   = *channel
        // close-connection = C:CLOSE S:CLOSEOK
        //                 / S:CLOSE C:CLOSEOK
        let rt = new_runtime();
        rt.block_on(async {
            let  (mut reader, mut writer) = SplitConnection::open("localhost:5672").await.unwrap();

            // C: protocol-header
            writer.write(&ProtocolHeader::default()).await.unwrap();

            // S: 'Start'
            let start = reader.read_frame().await.unwrap();
            println!(" {start:?}");

            // C: 'StartOk'
            let start_ok = StartOk::default().into_frame();
            writer.write_frame(0, start_ok).await.unwrap();

            // S: 'Tune'
            let tune = reader.read_frame().await.unwrap();
            println!("{tune:?}");

            // C: TuneOk
            let mut tune_ok = TuneOk::default();
            let tune = match tune.1 {
                Frame::Tune(_, v) => v,
                _ => panic!("wrong message"),
            };

            tune_ok.channel_max = tune.channel_max;
            tune_ok.frame_max = tune.frame_max;
            tune_ok.heartbeat = tune.heartbeat;

            writer.write_frame(0, tune_ok.into_frame()).await.unwrap();

            // C: Open
            let open = Open::default().into_frame();
            writer.write_frame(0, open).await.unwrap();

            // S: OpenOk
            let open_ok = reader.read_frame().await.unwrap();
            println!("{open_ok:?}");

            // C: Close
            writer.write_frame(0, Close::default().into_frame())
                .await
                .unwrap();

            // S: CloseOk
            let close_ok = reader.read_frame().await.unwrap();
            println!("{close_ok:?}");
        })
    }
}