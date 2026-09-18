use proc_macro::TokenStream;

/// No-op derive so `#[skippr(secret)]` / `secret_path` / `not_secret` compile on plugin configs.
#[proc_macro_derive(SkipprConfig, attributes(skippr))]
pub fn derive_skippr_config(_input: TokenStream) -> TokenStream {
    TokenStream::new()
}
