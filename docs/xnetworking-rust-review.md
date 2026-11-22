# XNetworking Rust review

Thanks for the work on XCL. This is a one-off contribution to help with the systems Rust side, not a commitment to become a recurring reviewer or PR author. Good job, everyone. I think Rust is the right choice and the project is on the right track. More than XboxtoWindows...

## First change to review

In `default_security_info_bytes`, stop copying the raw representation of `XNetworkingSecurityInformation`. Keep the output zero-initialized and write only the TLS flags at the field offset computed by `offset_of!`. Document the Windows x64 assumption for the zero count and null pointer. In the UTF-8 and UTF-16 tests, use aligned storage and assert all three result fields. Do not change async result consumption in this patch.

## Criteria for follow-up changes

Use SDK-backed Windows types. Keep unsafe operations at the FFI boundary and document extent, lifetime, aliasing and alignment. Validate runtime policy through the existing per-title config. Record fallbacks and unimplemented behavior through the existing logger without logging secrets. Keep ABI tests separate from Windows integration tests.

For AI-assisted work, give the agent the SDK signatures, ownership rules and acceptance tests before asking it to edit. Treat compiler and borrow-checker diagnostics as design feedback. Fix the type or ownership model instead of casting around the diagnostic, widening unsafe blocks, suppressing lints or weakening tests. Record which checks actually ran. No Windows build or tests were run for this review.
