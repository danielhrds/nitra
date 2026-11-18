//! Runtime HLSL compilation with `D3DCompile` (`d3dcompiler_47.dll`, part of Windows).

use std::ffi::CString;
use std::fmt;

use windows::Win32::Graphics::Direct3D::Fxc::{
    D3DCOMPILE_DEBUG, D3DCOMPILE_ENABLE_STRICTNESS, D3DCOMPILE_OPTIMIZATION_LEVEL3,
    D3DCOMPILE_SKIP_OPTIMIZATION, D3DCompile,
};
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::core::PCSTR;

/// Compiled shader bytecode.
#[derive(Clone, Debug)]
pub struct CompiledShader(ID3DBlob);

impl CompiledShader {
    pub fn bytecode(&self) -> &[u8] {
        // SAFETY: the blob owns a buffer of exactly GetBufferSize bytes that
        // lives as long as the blob, which `self` keeps alive.
        unsafe {
            std::slice::from_raw_parts(
                self.0.GetBufferPointer().cast::<u8>(),
                self.0.GetBufferSize(),
            )
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShaderError {
    pub entry_point: String,
    pub message: String,
}

impl fmt::Display for ShaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "shader {} failed to compile: {}",
            self.entry_point, self.message
        )
    }
}

impl std::error::Error for ShaderError {}

/// Compiles `entry_point` from HLSL `source` for `target` (e.g. `ps_5_0`).
/// Debug builds keep debug info and skip optimization.
pub fn compile_shader(
    source: &str,
    source_name: &str,
    entry_point: &str,
    target: &str,
) -> Result<CompiledShader, ShaderError> {
    let error = |message: String| ShaderError {
        entry_point: entry_point.to_owned(),
        message,
    };
    let to_c = |text: &str| CString::new(text).map_err(|_| error(format!("NUL in {text:?}")));
    let (name, entry, target) = (to_c(source_name)?, to_c(entry_point)?, to_c(target)?);

    let flags = D3DCOMPILE_ENABLE_STRICTNESS
        | if cfg!(debug_assertions) {
            D3DCOMPILE_DEBUG | D3DCOMPILE_SKIP_OPTIMIZATION
        } else {
            D3DCOMPILE_OPTIMIZATION_LEVEL3
        };

    let mut code = None;
    let mut messages = None;
    // SAFETY: the source pointer/length describe a live &str; the C strings
    // outlive the call; no macros or include handler are used; the out
    // pointers reference live locals.
    let result = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            PCSTR(name.as_ptr().cast()),
            None,
            None,
            PCSTR(entry.as_ptr().cast()),
            PCSTR(target.as_ptr().cast()),
            flags,
            0,
            &mut code,
            Some(&mut messages),
        )
    };

    match (result, code) {
        (Ok(()), Some(code)) => Ok(CompiledShader(code)),
        (result, _) => {
            let compiler_output = messages
                .map(|blob| CompiledShader(blob).bytecode().to_vec())
                .map(|bytes| {
                    String::from_utf8_lossy(&bytes)
                        .trim_end_matches('\0')
                        .to_owned()
                });
            Err(error(match (compiler_output, result) {
                (Some(text), _) if !text.trim().is_empty() => text,
                (_, Err(api_error)) => api_error.to_string(),
                _ => "no bytecode produced".to_owned(),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "float4 main() : SV_Target { return float4(1, 0, 0, 1); }";

    #[test]
    fn compiles_valid_pixel_shader() {
        let shader = compile_shader(VALID, "valid.hlsl", "main", "ps_5_0").unwrap();
        // DXBC containers start with the "DXBC" magic.
        assert_eq!(&shader.bytecode()[..4], b"DXBC");
    }

    #[test]
    fn reports_compiler_errors() {
        let error = compile_shader("float4 main( { }", "broken.hlsl", "main", "ps_5_0")
            .expect_err("must fail");
        assert_eq!(error.entry_point, "main");
        assert!(error.message.contains("broken.hlsl"), "{}", error.message);
    }
}
