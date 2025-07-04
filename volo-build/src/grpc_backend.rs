use itertools::Itertools;
use pilota_build::{
    CodegenBackend, Context, DefId, IdentName, Symbol,
    db::RirDatabase,
    rir,
    rir::Method,
    tags::protobuf::{ClientStreaming, ServerStreaming},
};
use volo::FastStr;

use crate::util::{get_base_dir, write_file, write_item};

pub struct MkGrpcBackend;

impl pilota_build::MakeBackend for MkGrpcBackend {
    type Target = VoloGrpcBackend;

    fn make_backend(self, context: Context) -> Self::Target {
        VoloGrpcBackend {
            inner: pilota_build::codegen::pb::ProtobufBackend::new(context),
        }
    }
}

#[derive(Clone)]
pub struct VoloGrpcBackend {
    inner: pilota_build::codegen::pb::ProtobufBackend,
}

impl VoloGrpcBackend {
    fn trait_input_ty(
        &self,
        ty: pilota_build::ty::Ty,
        streaming: bool,
        global_path: bool,
    ) -> FastStr {
        let ty = self.cx().codegen_item_ty(ty.kind);
        let ty_str = if global_path {
            format!("{}", ty.global_path("volo_gen"))
        } else {
            format!("{ty}")
        };

        if streaming {
            format!("::volo_grpc::Request<::volo_grpc::RecvStream<{ty_str}>>").into()
        } else {
            format!("::volo_grpc::Request<{ty_str}>").into()
        }
    }

    fn trait_output_ty(
        &self,
        ty: pilota_build::ty::Ty,
        streaming: bool,
        global_path: bool,
    ) -> FastStr {
        let ret_ty = self.cx().codegen_item_ty(ty.kind);
        let ret_ty_str = if global_path {
            format!("{}", ret_ty.global_path("volo_gen"))
        } else {
            format!("{ret_ty}")
        };

        if streaming {
            format!(
                "::volo_grpc::Response<::volo_grpc::BoxStream<'static, \
                 ::std::result::Result<{ret_ty_str}, ::volo_grpc::Status>>>, ::volo_grpc::Status"
            )
            .into()
        } else {
            format!("::volo_grpc::Response<{ret_ty_str}>, ::volo_grpc::Status").into()
        }
    }

    fn trait_result_ty(&self, streaming: bool) -> FastStr {
        if streaming {
            r#"
					use ::volo_grpc::codegen::StreamExt;
					let repeat = std::iter::repeat(Default::default());
					let mut resp = ::volo_grpc::codegen::iter(repeat);
					let (tx, rx) = ::volo_grpc::codegen::mpsc::channel(64);
					tokio::spawn(async move {
						while let Some(resp) = resp.next().await {
							match tx.send(::std::result::Result::<_, ::volo_grpc::Status>::Ok(resp)).await {
								::std::result::Result::Ok(_) => {}
								::std::result::Result::Err(_) => {
									break;
								}
							}
						}
					});
					::std::result::Result::Ok(::volo_grpc::Response::new(Box::pin(::volo_grpc::codegen::ReceiverStream::new(rx))))
				"#
            .into()
        } else {
            "::std::result::Result::Ok(::volo_grpc::Response::new(Default::default()))".into()
        }
    }

    fn client_input_ty(&self, ty: pilota_build::ty::Ty, streaming: bool) -> FastStr {
        let ty = self.cx().codegen_item_ty(ty.kind);

        if streaming {
            format!("impl ::volo_grpc::IntoStreamingRequest<Message = {ty}>").into()
        } else {
            format!("impl ::volo_grpc::IntoRequest<{ty}>").into()
        }
    }

    fn client_output_ty(&self, ty: pilota_build::ty::Ty, streaming: bool) -> FastStr {
        let ret_ty = self.cx().codegen_item_ty(ty.kind);

        if streaming {
            format!(
                "::std::result::Result<::volo_grpc::Response<impl \
                 ::volo_grpc::codegen::futures::Stream<Item = \
                 ::std::result::Result<{ret_ty},::volo_grpc::Status>>>, ::volo_grpc::Status>"
            )
            .into()
        } else {
            format!("::std::result::Result<::volo_grpc::Response<{ret_ty}>, ::volo_grpc::Status>")
                .into()
        }
    }

    fn build_client_req(&self, _ty: pilota_build::ty::Ty, streaming: bool) -> FastStr {
        if streaming {
            "requests.into_streaming_request().map(|s| ::volo_grpc::codegen::StreamExt::map(s, |m| \
             ::std::result::Result::Ok(m)))"
                .to_string()
                .into()
        } else {
            "requests.into_request().map(|m| \
             ::volo_grpc::codegen::futures::stream::once(::volo_grpc::codegen::futures::future::ready(::std::result::Result::Ok(m))))"
                .to_string()
                .into()
        }
    }

    fn build_client_resp(
        &self,
        resp_enum_name: &Symbol,
        variant_name: &Symbol,
        _ty: pilota_build::ty::Ty,
        streaming: bool,
    ) -> FastStr {
        let resp_stream = format!(
            r#"let (mut metadata, extensions, message_stream) = resp.into_parts();
            let mut message_stream = match message_stream {{
                {resp_enum_name}::{variant_name}(stream) => stream,
                #[allow(unreachable_patterns)]
                _ => return ::std::result::Result::Err(::volo_grpc::Status::new(::volo_grpc::Code::Unimplemented, "Method not found.")),
            }};"#
        );

        if streaming {
            format! {
                r#"{resp_stream}
                ::std::result::Result::Ok(::volo_grpc::Response::from_parts(metadata, extensions, message_stream))"#
            }
        } else {
            format! {
                r#"{resp_stream}
                let message = ::volo_grpc::codegen::StreamExt::try_next(&mut message_stream)
                    .await
                    .map_err(|mut status| {{
                        status.metadata_mut().merge(metadata.clone());
                        status
                    }})?
                    .ok_or_else(|| ::volo_grpc::Status::new(::volo_grpc::Code::Internal, "Missing response message."))?;
                if let Some(trailers) = message_stream.trailers().await? {{
                    metadata.merge(trailers);
                }}
                ::std::result::Result::Ok(::volo_grpc::Response::from_parts(metadata, extensions, message))"#
            }
        }.into()
    }

    fn build_server_req(
        &self,
        req_enum_name: &Symbol,
        variant_name: &Symbol,
        _ty: pilota_build::ty::Ty,
        streaming: bool,
    ) -> FastStr {
        let req_stream = format!(
            r#"let (mut metadata, extensions, message_stream) = req.into_parts();
            let mut message_stream = match message_stream {{
                {req_enum_name}::{variant_name}(stream) => stream,
                #[allow(unreachable_patterns)]
                _ => return ::std::result::Result::Err(::volo_grpc::Status::new(::volo_grpc::Code::Unimplemented, "Method not found.")),
            }};"#
        );
        if streaming {
            format! {
                r#"{req_stream}
                let req = ::volo_grpc::Request::from_parts(metadata, extensions, message_stream);"#
            }
        } else {
            format! {
                r#"{req_stream}
                ::volo_grpc::codegen::futures::pin_mut!(message_stream);
                let message = ::volo_grpc::codegen::StreamExt::try_next(&mut message_stream)
                    .await?
                    .ok_or_else(|| ::volo_grpc::Status::new(::volo_grpc::Code::Internal, "Missing request message."))?;
                if let Some(trailers) = message_stream.trailers().await? {{
                    metadata.merge(trailers);
                }}
                let req = ::volo_grpc::Request::from_parts(metadata, extensions, message);"#
            }
        }.into()
    }

    fn build_server_call(&self, method: &Method) -> FastStr {
        let method_name = self.cx().rust_name(method.def_id);
        format!("let resp = inner.{method_name}(req).await;").into()
    }

    fn build_server_resp(
        &self,
        resp_enum_name: &Symbol,
        variant_name: &Symbol,
        _ty: pilota_build::ty::Ty,
        streaming: bool,
    ) -> FastStr {
        if streaming {
            format!("resp.map(|r| r.map(|s|  {resp_enum_name}::{variant_name}(s)))").into()
        } else {
            format!(
                "resp.map(|r| r.map(|m| {resp_enum_name}::{variant_name}(::std::boxed::Box::pin( \
                 ::volo_grpc::codegen::futures::stream::once(::volo_grpc::codegen::futures::future::ok(m))))))"
            )
            .into()
        }
    }
}

impl CodegenBackend for VoloGrpcBackend {
    const PROTOCOL: &'static str = "protobuf";

    fn codegen_service_impl(&self, def_id: DefId, stream: &mut String, s: &rir::Service) {
        let service_name = self.cx().rust_name(def_id);
        let server_name = format!("{service_name}Server");
        let client_builder_name = format!("{service_name}ClientBuilder");
        let generic_client_name = format!("{service_name}GenericClient");
        let client_name = format!("{service_name}Client");
        let oneshot_client_name = format!("{service_name}OneShotClient");

        let file_id = self.cx().node(def_id).unwrap().file_id;
        let file = self.cx().file(file_id).unwrap();

        let package = file.package.iter().join(".");
        let name = format!("{package}.{}", s.name);

        let req_enum_name_send = format!("{service_name}RequestSend");
        let resp_enum_name_send = format!("{service_name}ResponseSend");
        let req_enum_name_recv = format!("{service_name}RequestRecv");
        let resp_enum_name_recv = format!("{service_name}ResponseRecv");

        let path = self.cx().item_path(def_id);
        let path = path.as_ref();
        let buf = get_base_dir(self.cx().mode.as_ref(), self.cx().names.get(&def_id), path);
        let base_dir = buf.as_path();

        if self.cx().split {
            std::fs::create_dir_all(base_dir).expect("Failed to create base directory");
        }

        let paths = s
            .methods
            .iter()
            .map(|method| format!("/{package}.{}/{}", s.name, method.name))
            .collect::<Vec<_>>();

        let req_matches = s
            .methods
            .iter()
            .map(|method| {
                let variant_name = self.cx().rust_name(method.def_id).0.upper_camel_ident();
                let path = format!("/{package}.{}/{}", s.name, method.name);
                let client_streaming = self
                    .cx()
                    .node_contains_tag::<ClientStreaming>(method.def_id);
                let input_ty = &method.args[0].ty;

                let server_streaming = self
                    .cx()
                    .node_contains_tag::<ServerStreaming>(method.def_id);
                let output_ty = &method.ret;

                let req = self.build_server_req(
                    &req_enum_name_recv.clone().into(),
                    &variant_name.clone().into(),
                    input_ty.clone(),
                    client_streaming,
                );

                let call = self.build_server_call(method);

                let resp = self.build_server_resp(
                    &resp_enum_name_send.clone().into(),
                    &variant_name.into(),
                    output_ty.clone(),
                    server_streaming,
                );

                format! {
                    r#""{path}" => {{
                    {req}
                    {call}
                    {resp}
                }},"#
                }
            })
            .join("");

        let enum_variant_names = s
            .methods
            .iter()
            .map(|method| self.cx().rust_name(method.def_id).0.upper_camel_ident())
            .collect::<Vec<_>>();

        let req_tys = s
            .methods
            .iter()
            .map(|method| self.cx().codegen_item_ty(method.args[0].ty.kind.clone()))
            .collect::<Vec<_>>();
        let resp_tys = s
            .methods
            .iter()
            .map(|method| self.cx().codegen_item_ty(method.ret.kind.clone()))
            .collect::<Vec<_>>();

        let mut client_methods = Vec::new();
        let mut oneshot_client_methods = Vec::new();

        s.methods.iter().for_each(|method| {
            let method_name = self.cx().rust_name(method.def_id);

            let path = format!("/{package}.{}/{}", s.name, method.name);
            let input_ty = &method.args[0].ty;
            let client_streaming = self.cx().node_contains_tag::<ClientStreaming>(method.def_id);
            let req_ty = self.client_input_ty(input_ty.clone(), client_streaming);

            let output_ty = &method.ret;
            let server_streaming = self.cx().node_contains_tag::<ServerStreaming>(method.def_id);

            let variant_name = self.cx().rust_name(method.def_id).0.upper_camel_ident();

            let resp_ty = self.client_output_ty(output_ty.clone(), server_streaming);

            let req = self.build_client_req(input_ty.clone(), client_streaming);

            let resp = self.build_client_resp(&resp_enum_name_recv.clone().into(), &variant_name.clone().into(), output_ty.clone(), server_streaming);

            client_methods.push(
                format! {
                    r#"pub async fn {method_name}(
                        &self,
                        requests: {req_ty},
                    ) -> {resp_ty} {{
                        let req = {req}.map(|message| {req_enum_name_send}::{variant_name}(::std::boxed::Box::pin(message) as _));
                        let mut cx = self.0.make_cx("{path}");

                        let resp = ::volo::Service::call(&self.0, &mut cx, req).await?;
                        {resp}
                    }}"#
                }
            );

            oneshot_client_methods.push(
                format! {
                    r#"pub async fn {method_name}(
                        self,
                        requests: {req_ty},
                    ) -> {resp_ty} {{
                        let req = {req}.map(|message| {req_enum_name_send}::{variant_name}(::std::boxed::Box::pin(message) as _));
                        let mut cx = self.0.make_cx("{path}");

                        let resp = ::volo::client::OneShotService::call(self.0, &mut cx, req).await?;

                        {resp}
                    }}"#
                }
            );
        });

        let mk_client_name = format!("Mk{generic_client_name}");

        let client_methods = client_methods.join("\n");
        let oneshot_client_methods = oneshot_client_methods.join("\n");

        let req_enum_send_variants = crate::join_multi_strs!(
            "\n",
            |enum_variant_names, req_tys| -> "{enum_variant_names}(::volo_grpc::BoxStream<'static, ::std::result::Result<{req_tys}, ::volo_grpc::Status>>),"
        );

        let req_enum_recv_variants = crate::join_multi_strs!(
            "\n",
            |enum_variant_names, req_tys| -> "{enum_variant_names}(::volo_grpc::RecvStream<{req_tys}>),"
        );

        let resp_enum_send_variants = crate::join_multi_strs!(
            "\n",
            |enum_variant_names, resp_tys| -> "{enum_variant_names}(::volo_grpc::BoxStream<'static, ::std::result::Result<{resp_tys}, ::volo_grpc::Status>>),"
        );

        let resp_enum_recv_variants = crate::join_multi_strs!(
            "\n",
            |enum_variant_names, resp_tys| -> "{enum_variant_names}(::volo_grpc::RecvStream<{resp_tys}>),"
        );

        let req_send_into_body = crate::join_multi_strs!(
            "",
            |enum_variant_names| -> "Self::{enum_variant_names}(s) => {{
                ::volo_grpc::codec::encode::encode(s, compression_encoding)
            }},"
        );

        let req_recv_from_body = crate::join_multi_strs!(
            "",
            |paths, enum_variant_names| -> "Some(\"{paths}\") => {{
                ::std::result::Result::Ok(Self::{enum_variant_names}(::volo_grpc::RecvStream::new(body, kind,compression_encoding)))
            }},"
        );

        let resp_send_into_body = crate::join_multi_strs!(
            "",
            |enum_variant_names| -> "Self::{enum_variant_names}(s) => {{
                ::volo_grpc::codec::encode::encode(s,compression_encoding)
            }},"
        );

        let resp_recv_from_body = crate::join_multi_strs!(
            "",
            |paths, enum_variant_names| -> "Some(\"{paths}\") => {{
                ::std::result::Result::Ok(Self::{enum_variant_names}(::volo_grpc::RecvStream::new(body, kind, compression_encoding)))
            }}"
        );

        let req_enum_send_impl = format! {
            r#"
            pub enum {req_enum_name_send} {{
                {req_enum_send_variants}
            }}

            impl ::volo_grpc::SendEntryMessage for {req_enum_name_send} {{
                fn into_body(self,compression_encoding: ::std::option::Option<::volo_grpc::codec::compression::CompressionEncoding>) -> ::volo_grpc::BoxStream<'static, ::std::result::Result<::volo_grpc::codegen::Frame<::volo_grpc::codegen::Bytes>, ::volo_grpc::Status>> {{
                    match self {{
                        {req_send_into_body}
                    }}
                }}
            }}"#
        };

        let req_enum_recv_impl = format! {
            r#"
            pub enum {req_enum_name_recv} {{
                {req_enum_recv_variants}
            }}

            impl ::volo_grpc::RecvEntryMessage for {req_enum_name_recv} {{
                fn from_body(method: ::std::option::Option<&str>, body: ::volo_grpc::body::BoxBody, kind: ::volo_grpc::codec::decode::Kind,compression_encoding: ::std::option::Option<::volo_grpc::codec::compression::CompressionEncoding>) -> ::std::result::Result<Self, ::volo_grpc::Status> {{
                    match method {{
                        {req_recv_from_body}
                        _ => ::std::result::Result::Err(::volo_grpc::Status::new(::volo_grpc::Code::Unimplemented, "Method not found.")),
                    }}
                }}
            }}"#
        };

        let resp_enum_send_impl = format! {
            r#"
            pub enum {resp_enum_name_send} {{
                {resp_enum_send_variants}
            }}

            impl ::volo_grpc::SendEntryMessage for {resp_enum_name_send} {{
                fn into_body(self,compression_encoding: ::std::option::Option<::volo_grpc::codec::compression::CompressionEncoding>) -> ::volo_grpc::BoxStream<'static, ::std::result::Result<::volo_grpc::codegen::Frame<::volo_grpc::codegen::Bytes>, ::volo_grpc::Status>> {{
                    match self {{
                        {resp_send_into_body}
                    }}
                }}
            }}"#
        };

        let resp_enum_recv_impl = format! {
            r#"
            pub enum {resp_enum_name_recv} {{
                {resp_enum_recv_variants}
            }}

            impl ::volo_grpc::RecvEntryMessage for {resp_enum_name_recv} {{
                fn from_body(method: ::std::option::Option<&str>, body: ::volo_grpc::body::BoxBody, kind: ::volo_grpc::codec::decode::Kind,compression_encoding: ::std::option::Option<::volo_grpc::codec::compression::CompressionEncoding>) -> ::std::result::Result<Self, ::volo_grpc::Status>
                where
                    Self: ::core::marker::Sized,
                {{
                    match method {{
                        {resp_recv_from_body}
                        _ => ::std::result::Result::Err(::volo_grpc::Status::new(::volo_grpc::Code::Unimplemented, "Method not found.")),
                    }}
                }}
            }}"#
        };

        let client_impl = format! {
            r#"
            pub struct {client_builder_name} {{}}
            impl {client_builder_name} {{
                pub fn new(
                    service_name: impl AsRef<str>,
                ) -> ::volo_grpc::client::ClientBuilder<
                    ::volo::layer::Identity,
                    ::volo::layer::Identity,
                    {mk_client_name},
                    ::volo_grpc::layer::loadbalance::LbConfig<::volo::loadbalance::random::WeightedRandomBalance<(::volo::FastStr)>, ::volo_grpc::client::dns::DnsResolver>,
                    {req_enum_name_send},
                    {resp_enum_name_recv},
                > {{
                    ::volo_grpc::client::ClientBuilder::new({mk_client_name}, service_name)
                }}
            }}

            pub struct {mk_client_name};

            pub type {client_name} = {generic_client_name}<::volo::service::BoxCloneService<::volo_grpc::context::ClientContext, ::volo_grpc::Request<{req_enum_name_send}>, ::volo_grpc::Response<{resp_enum_name_recv}>, ::volo_grpc::Status>>;

            impl<S> ::volo::client::MkClient<::volo_grpc::Client<S>> for {mk_client_name} {{
                type Target = {generic_client_name}<S>;
                fn mk_client(&self, service: ::volo_grpc::Client<S>) -> Self::Target {{
                    {generic_client_name}(service)
                }}
            }}

            #[derive(Clone)]
            pub struct {generic_client_name}<S>(pub ::volo_grpc::Client<S>);

            pub struct {oneshot_client_name}<S>(pub ::volo_grpc::Client<S>);

            impl<S> {generic_client_name}<S> where S: ::volo::service::Service<::volo_grpc::context::ClientContext, ::volo_grpc::Request<{req_enum_name_send}>, Response=::volo_grpc::Response<{resp_enum_name_recv}>, Error = ::volo_grpc::Status> + Sync + Send + 'static {{
                pub fn with_callopt<Opt: ::volo::client::Apply<::volo_grpc::context::ClientContext>>(self, opt: Opt) -> {oneshot_client_name}<::volo::client::WithOptService<S, Opt>> {{
                    {oneshot_client_name}(self.0.with_opt(opt))
                }}

                {client_methods}
            }}

            impl<S: ::volo::client::OneShotService<::volo_grpc::context::ClientContext,::volo_grpc::Request<{req_enum_name_send}>, Response=::volo_grpc::Response<{resp_enum_name_recv}>, Error = ::volo_grpc::Status> + Send + Sync + 'static> {oneshot_client_name}<S> {{
                {oneshot_client_methods}
            }}"#
        };

        let server_impl = format! {
            r#"
            pub struct {server_name}<S> {{
                inner: ::std::sync::Arc<S>,
            }}

            impl<S> Clone for {server_name}<S> {{
                fn clone(&self) -> Self {{
                    {server_name} {{
                        inner: self.inner.clone(),
                    }}
                }}
            }}

            impl<S> {server_name}<S> {{
                pub fn new(inner: S) -> Self {{
                    Self::from_arc(::std::sync::Arc::new(inner))
                }}

                pub fn from_arc(inner: ::std::sync::Arc<S>) -> Self {{
                    Self {{
                        inner,
                    }}
                }}
            }}

            impl<S> ::volo::service::Service<::volo_grpc::context::ServerContext, ::volo_grpc::Request<{req_enum_name_recv}>> for {server_name}<S>
            where
                S: {service_name} + ::core::marker::Send + ::core::marker::Sync + 'static,
            {{
                type Response = ::volo_grpc::Response<{resp_enum_name_send}>;
                type Error = ::volo_grpc::status::Status;

                async fn call<'s, 'cx>(&'s self, cx: &'cx mut ::volo_grpc::context::ServerContext, req: ::volo_grpc::Request<{req_enum_name_recv}>) -> ::std::result::Result<Self::Response, Self::Error> {{
                    let inner = self.inner.clone();
                    match cx.rpc_info.method().as_str() {{
                        {req_matches}
                        path => {{
                            let path = path.to_string();
                            ::std::result::Result::Err(::volo_grpc::Status::unimplemented(::std::format!("Unimplemented http path: {{}}", path)))
                        }}
                    }}
                }}
            }}

            impl<S: {service_name}> ::volo_grpc::server::NamedService for {server_name}<S> {{
                const NAME: &'static str = "{name}";
            }}"#
        };

        if self.cx().split {
            let mut mod_rs_stream = String::new();
            write_item(
                &mut mod_rs_stream,
                base_dir,
                format!("enum_{req_enum_name_send}.rs"),
                req_enum_send_impl,
            );
            write_item(
                &mut mod_rs_stream,
                base_dir,
                format!("enum_{req_enum_name_recv}.rs"),
                req_enum_recv_impl,
            );
            write_item(
                &mut mod_rs_stream,
                base_dir,
                format!("enum_{resp_enum_name_send}.rs"),
                resp_enum_send_impl,
            );
            write_item(
                &mut mod_rs_stream,
                base_dir,
                format!("enum_{resp_enum_name_recv}.rs"),
                resp_enum_recv_impl,
            );

            write_item(
                &mut mod_rs_stream,
                base_dir,
                format!("client_{client_name}.rs"),
                client_impl,
            );
            write_item(
                &mut mod_rs_stream,
                base_dir,
                format!("server_{server_name}.rs"),
                server_impl,
            );

            let mod_rs_file_path = base_dir.join("mod.rs");
            write_file(&mod_rs_file_path, mod_rs_stream);
            stream.push_str(
                format!(
                    "include!(\"{}/mod.rs\");",
                    base_dir.file_name().unwrap().to_str().unwrap()
                )
                .as_str(),
            );
        } else {
            stream.push_str(&format! {
                r#"
            {req_enum_send_impl}
            {req_enum_recv_impl}
            {resp_enum_send_impl}
            {resp_enum_recv_impl}

            {client_impl}
            {server_impl}
            "#});
        }
    }

    fn codegen_service_method(&self, _service_def_id: DefId, method: &rir::Method) -> String {
        let client_streaming = self
            .cx()
            .node_contains_tag::<ClientStreaming>(method.def_id);
        let args = method
            .args
            .iter()
            .map(|a| {
                let ty = self.trait_input_ty(a.ty.clone(), client_streaming, false);

                let ident = &a.name;
                format!("{ident}: {ty}")
            })
            .join(",");

        let ret_ty = self.trait_output_ty(
            method.ret.clone(),
            self.cx()
                .node_contains_tag::<ServerStreaming>(method.def_id),
            false,
        );

        let name = self.cx().rust_name(method.def_id);

        format!(
            "fn {name}(&self, {args}) -> impl ::std::future::Future<Output = \
             ::std::result::Result<{ret_ty}>> + Send;"
        )
    }

    fn codegen_service_method_with_global_path(
        &self,
        _service_def_id: DefId,
        method: &Method,
    ) -> String {
        let client_streaming = self
            .cx()
            .node_contains_tag::<ClientStreaming>(method.def_id);
        let args = method
            .args
            .iter()
            .map(|a| {
                let ty = self.trait_input_ty(a.ty.clone(), client_streaming, true);

                let ident = &a.name;
                // args are unused, add _ to avoid unused variable warning
                format!("_{ident}: {ty}")
            })
            .join(",");

        let server_streaming = self
            .cx()
            .node_contains_tag::<ServerStreaming>(method.def_id);
        let ret_ty = self.trait_output_ty(method.ret.clone(), server_streaming, true);

        let default_result = self.trait_result_ty(server_streaming);

        let name = self.cx().rust_name(method.def_id);

        format!(
            r#"
    async fn {name}(
        &self,
        {args},
    ) -> ::std::result::Result<{ret_ty}>
    {{
        {default_result}
    }}
"#
        )
    }

    fn codegen_enum_impl(&self, def_id: DefId, stream: &mut String, e: &rir::Enum) {
        self.inner.codegen_enum_impl(def_id, stream, e)
    }

    fn codegen_newtype_impl(&self, def_id: DefId, stream: &mut String, t: &rir::NewType) {
        self.inner.codegen_newtype_impl(def_id, stream, t)
    }

    fn codegen_struct_impl(&self, def_id: DefId, stream: &mut String, s: &rir::Message) {
        self.inner.codegen_struct_impl(def_id, stream, s)
    }

    fn cx(&self) -> &Context {
        self.inner.cx()
    }

    fn codegen_pilota_buf_trait(&self, stream: &mut String) {
        self.inner.codegen_pilota_buf_trait(stream)
    }
}
