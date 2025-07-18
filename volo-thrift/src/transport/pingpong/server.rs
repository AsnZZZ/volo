use std::{
    cell::RefCell,
    sync::{Arc, atomic::Ordering},
};

use metainfo::MetaInfo;
use motore::service::Service;
use pilota::thrift::ThriftException;
use tokio::sync::futures::Notified;
use tracing::*;
use volo::{net::Address, volo_unreachable};

use crate::{
    DummyMessage, EntryMessage, ServerError, ThriftMessage,
    codec::{Decoder, Encoder},
    context::{SERVER_CONTEXT_CACHE, ServerContext, ThriftContext},
    protocol::TMessageType,
    server_error_to_application_exception, thrift_exception_to_application_exception,
    tracing::SpanProvider,
    transport::should_log,
};

#[allow(clippy::too_many_arguments)]
pub async fn serve<Svc, Req, Resp, E, D, SP>(
    mut encoder: E,
    mut decoder: D,
    notified: Notified<'_>,
    exit_mark: Arc<std::sync::atomic::AtomicBool>,
    service: &Svc,
    stat_tracer: Arc<[crate::server::TraceFn]>,
    peer_addr: Option<Address>,
    span_provider: SP,
) where
    Svc: Service<ServerContext, Req, Response = Resp>,
    Svc::Error: Into<ServerError>,
    Req: EntryMessage,
    Resp: EntryMessage,
    E: Encoder,
    D: Decoder,
    SP: SpanProvider,
{
    tokio::pin!(notified);

    metainfo::METAINFO
        .scope(RefCell::new(MetaInfo::default()), async {
            loop {
                // new context
                let mut cx = SERVER_CONTEXT_CACHE.with(|cache| {
                    let mut cache = cache.borrow_mut();
                    cache.pop().unwrap_or_default()
                });
                if let Some(peer_addr) = &peer_addr {
                    cx.rpc_info.caller_mut().set_address(peer_addr.clone());
                }

                let msg = tokio::select! {
                    _ = &mut notified => {
                        tracing::trace!(
                            "[VOLO] close conn by notified, peer_addr: {:?}",
                            peer_addr,
                        );
                        return;
                    },
                    out = decoder.decode(&mut cx) => out
                };
                debug!(
                    "[VOLO] received message: {:?}, cx: {:?}, peer_addr: {:?}",
                    msg.as_ref().map(|msg| msg.as_ref().map(|msg| &msg.meta)),
                    cx,
                    peer_addr
                );

                // it is promised safe here, because span only reads cx before handling polling
                let tracing_cx = unsafe {
                    std::mem::transmute::<
                        &crate::context::ServerContext,
                        &crate::context::ServerContext,
                    >(&cx)
                };

                let result = async {
                    match msg {
                        Ok(Some(ThriftMessage { data: Ok(req), .. })) => {
                            cx.stats.record_process_start_at();
                            let resp = service.call(&mut cx, req).await.map_err(Into::into);
                            cx.stats.record_process_end_at();

                            if exit_mark.load(Ordering::Relaxed) {
                                cx.set_conn_reset_by_ttheader(true);
                            }

                            let req_msg_type =
                                cx.req_msg_type.expect("`req_msg_type` should be set.");

                            if req_msg_type != TMessageType::OneWay {
                                cx.msg_type = Some(match resp {
                                    Ok(_) => TMessageType::Reply,
                                    Err(_) => TMessageType::Exception,
                                });
                                let msg = ThriftMessage::mk_server_resp(
                                    &cx,
                                    resp.map_err(server_error_to_application_exception),
                                );
                                if let Err(e) = async {
                                    let result = encoder.encode(&mut cx, msg).await;
                                    span_provider.leave_encode(&cx);
                                    result
                                }
                                .instrument(span_provider.on_encode(tracing_cx))
                                .await
                                {
                                    if should_log(&e) {
                                        error!(
                                            "[VOLO] server send response error: {:?}, cx: {:?}, \
                                             peer_addr: {:?}",
                                            e, cx, peer_addr
                                        );
                                    }
                                    stat_tracer.iter().for_each(|f| f(&cx));
                                    return Err(());
                                }
                            }
                            if cx.transport.is_conn_reset() {
                                return Err(());
                            }
                        }
                        Ok(Some(ThriftMessage { data: Err(_), .. })) => {
                            volo_unreachable!();
                        }
                        Ok(None) => {
                            trace!(
                                "[VOLO] reach eof, connection has been closed by client, \
                                 peer_addr: {:?}",
                                peer_addr
                            );
                            return Err(());
                        }
                        Err(e) => {
                            if should_log(&e) {
                                error!(
                                    "[VOLO] pingpong server decode error: {:?}, cx: {:?}, \
                                     peer_addr: {:?}",
                                    e, cx, peer_addr
                                );
                            }
                            cx.msg_type = Some(TMessageType::Exception);
                            cx.set_conn_reset_by_ttheader(true);
                            if !matches!(e, ThriftException::Transport(_)) {
                                let msg = ThriftMessage::mk_server_resp(
                                    &cx,
                                    Err::<DummyMessage, _>(
                                        thrift_exception_to_application_exception(e),
                                    ),
                                );
                                if let Err(e) = encoder.encode(&mut cx, msg).await {
                                    if should_log(&e) {
                                        error!(
                                            "[VOLO] server send error error: {:?}, cx: {:?}, \
                                             peer_addr: {:?}",
                                            e, cx, peer_addr
                                        );
                                    }
                                }
                            }
                            stat_tracer.iter().for_each(|f| f(&cx));
                            return Err(());
                        }
                    }
                    stat_tracer.iter().for_each(|f| f(&cx));

                    metainfo::METAINFO.with(|mi| {
                        mi.borrow_mut().clear();
                    });

                    span_provider.leave_serve(&cx);
                    SERVER_CONTEXT_CACHE.with(|cache| {
                        let mut cache = cache.borrow_mut();
                        if cache.len() < cache.capacity() {
                            cx.reset(Default::default());
                            cache.push(cx);
                        }
                    });
                    Ok(())
                }
                .instrument(span_provider.on_serve(tracing_cx))
                .await;
                if result.is_err() {
                    break;
                }
            }
        })
        .await;
}
