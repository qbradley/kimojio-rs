// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Kimojio procedural macros.
//!
//! These macros provide convenient attributes for defining async main functions and tests
//! that run within the kimojio runtime.

use proc_macro::TokenStream;
use quote::quote;
use syn::{ItemFn, parse_macro_input};

/// Marks an async main function to be run with the kimojio runtime.
///
/// This macro transforms an async main function into a regular main function
/// that uses `kimojio::run` to execute the async code.
///
/// # Example
///
/// ```ignore
/// #[kimojio::main]
/// async fn main() {
///     // async main code
/// }
/// ```
///
/// This expands to:
///
/// ```ignore
/// fn main() {
///     kimojio::run(0, async {
///         // async main code
///     });
/// }
/// ```
#[proc_macro_attribute]
pub fn main(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);

    let ItemFn {
        attrs,
        vis,
        sig,
        block,
    } = input;

    let fn_name = &sig.ident;

    // Check if function is async
    if sig.asyncness.is_none() {
        return syn::Error::new_spanned(
            sig.fn_token,
            "the async keyword is missing from the function declaration",
        )
        .to_compile_error()
        .into();
    }

    // Check if it's the main function
    if fn_name != "main" {
        return syn::Error::new_spanned(
            fn_name,
            "only the main function can be marked with #[kimojio::main]",
        )
        .to_compile_error()
        .into();
    }

    // Check that main has no parameters
    if !sig.inputs.is_empty() {
        return syn::Error::new_spanned(&sig.inputs, "the main function cannot have parameters")
            .to_compile_error()
            .into();
    }

    // Get the function body without async
    let body = &block;

    // Get the return type from the async function signature
    let return_type = &sig.output;

    // Generate the expanded code
    let expanded = quote! {
        #(#attrs)*
        #vis fn main() #return_type {
            match kimojio::run(0, async #body) {
                // Propagate the body return value
                Some(Ok(output)) => output,
                // Task panicked.
                Some(Err(panic_payload)) => std::panic::resume_unwind(panic_payload),
                None => panic!("Runtime shutdown_loop called"),
            }
        }
    };

    TokenStream::from(expanded)
}

/// Marks an async test to be run with the kimojio runtime.
///
/// This macro transforms an async test function into a regular test function
/// that uses `::kimojio::run_test` to execute the async code.
///
/// # Example
///
/// ```ignore
/// #[kimojio::test]
/// async fn my_test() {
///     // async test code
/// }
/// ```
///
/// ## Virtual Clock Mode
///
/// When the `virtual-clock` feature is enabled, the `virtual` parameter
/// creates a runtime with a [`VirtualClock`] and passes it to the test.
/// **Requires** the `virtual-clock` feature on the `kimojio` dependency.
///
/// ```ignore
/// #[kimojio::test(virtual)]
/// async fn my_test(clock: VirtualClock) {
///     clock.advance(Duration::from_secs(60));
///     // No real time passes!
/// }
/// ```
#[proc_macro_attribute]
pub fn test(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);

    let ItemFn {
        attrs,
        vis,
        sig,
        block,
    } = input;

    let fn_name = &sig.ident;
    let fn_name_str = fn_name.to_string();

    // Check if function is async
    if sig.asyncness.is_none() {
        return syn::Error::new_spanned(
            sig.fn_token,
            "the async keyword is missing from the function declaration",
        )
        .to_compile_error()
        .into();
    }

    // Parse attribute for "virtual" keyword.
    // Note: `virtual` is a reserved keyword in Rust, so syn::parse::<Ident>
    // won't work. We parse the token stream and check the string representation.
    let is_virtual = if attr.is_empty() {
        false
    } else {
        let tokens: Vec<_> = attr.clone().into_iter().collect();
        if tokens.len() == 1 {
            let token_str = tokens[0].to_string();
            if token_str == "virtual" {
                true
            } else {
                return syn::Error::new(
                    proc_macro::Span::call_site().into(),
                    format!(
                        "unsupported attribute `{token_str}`; expected `#[kimojio::test]` or `#[kimojio::test(virtual)]`"
                    ),
                )
                .to_compile_error()
                .into();
            }
        } else {
            return syn::Error::new(
                proc_macro::Span::call_site().into(),
                "unsupported attribute; expected `#[kimojio::test]` or `#[kimojio::test(virtual)]`",
            )
            .to_compile_error()
            .into();
        }
    };

    // Get the function body without async
    let body = &block;

    if is_virtual {
        // Virtual mode: test function should accept a VirtualClock parameter
        // Extract the parameter name (first param)
        let clock_param = if sig.inputs.len() == 1 {
            let param = sig.inputs.first().unwrap();
            match param {
                syn::FnArg::Typed(pat_type) => Some(pat_type.pat.clone()),
                _ => None,
            }
        } else if sig.inputs.is_empty() {
            None
        } else {
            return syn::Error::new_spanned(
                &sig.inputs,
                "virtual test function must have zero or one parameter (VirtualClock)",
            )
            .to_compile_error()
            .into();
        };

        let expanded = if let Some(clock_name) = clock_param {
            quote! {
                #[test]
                #(#attrs)*
                #vis fn #fn_name() {
                    ::kimojio::run_test_virtual(#fn_name_str, |#clock_name| async move #body)
                }
            }
        } else {
            // No parameter — pass clock but don't bind it
            quote! {
                #[test]
                #(#attrs)*
                #vis fn #fn_name() {
                    ::kimojio::run_test_virtual(#fn_name_str, |_clock| async move #body)
                }
            }
        };

        TokenStream::from(expanded)
    } else {
        // Standard mode — existing behavior
        let expanded = quote! {
            #[test]
            #(#attrs)*
            #vis fn #fn_name() {
                ::kimojio::run_test(#fn_name_str, async #body)
            }
        };

        TokenStream::from(expanded)
    }
}
