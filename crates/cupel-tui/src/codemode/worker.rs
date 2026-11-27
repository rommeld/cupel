use std::io::{self, Read, Write};
use std::process::ExitCode;

use monty::{FunctionCall, MontyRun, RunProgress};
use monty_types::{
    CompileOptions, DateTimeSource, ExcType, MontyException, MontyObject, ObjectRef, OsPolicy,
    PrintWriter, RandomStart, ResourceLimits, ResourceTracker, SleepMode,
};
use serde_json::Value;

use crate::codemode::{
    ALLOCATOR_BYTES, ARGUMENT_BYTES, CALLBACK_BYTES, CALLBACKS, EMIT_BYTES, Failure,
    GUEST_HEAP_BYTES, GUEST_TIME, SOURCE_BYTES, frame,
};

#[global_allocator]
static ALLOCATOR: monty_alloc::LimitedAllocator = monty_alloc::LimitedAllocator;

/// Run one script as a private stdio worker, before startup loads any configuration.
pub fn worker_main() -> ExitCode {
    let mut output = io::stdout().lock();
    let result = monty_alloc::set_hard_limit(Some(ALLOCATOR_BYTES))
        .map_err(|_error| Failure::Worker)
        .and_then(|()| frame::write(&mut output, frame::READY, &[]))
        .and_then(|()| execute(&mut io::stdin().lock(), &mut output));
    let terminal = match result {
        Ok(()) => frame::write(&mut output, frame::DONE, &[]),
        Err(error) => frame::write(&mut output, frame::FAILURE, &[frame::error_code(error)]),
    };
    if terminal.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn execute(input: &mut impl Read, output: &mut impl Write) -> Result<(), Failure> {
    let (_, source) = frame::read(input, |tag| match tag {
        frame::SOURCE => Ok(SOURCE_BYTES),
        _ => Err(Failure::Worker),
    })?;
    let source = String::from_utf8(source).map_err(|_error| Failure::Compile)?;
    let functions = ["read", "grep", "emit"];
    let runner = MontyRun::new(
        source,
        "codemode.py",
        functions.map(str::to_owned).to_vec(),
        CompileOptions::default(),
    )
    .map_err(|_error| Failure::Compile)?
    .with_os_policy(OsPolicy {
        datetime: DateTimeSource::CallHost,
        sleep: SleepMode::CallHost,
        random_start: RandomStart::CallHost,
        ..OsPolicy::default()
    });
    let limits = ResourceLimits::default()
        .max_memory(GUEST_HEAP_BYTES)
        .max_feed_duration(GUEST_TIME);
    let inputs = functions
        .map(|name| MontyObject::function(name, None))
        .to_vec();
    let mut progress = runner
        .start(inputs, ResourceTracker::new(limits), PrintWriter::Disabled)
        .map_err(exception)?;
    let mut callback_count = 0;
    let mut callback_bytes = CALLBACK_BYTES;
    let mut emit_bytes = EMIT_BYTES;
    loop {
        let call = match progress {
            RunProgress::Complete(_) => return Ok(()),
            RunProgress::FunctionCall(call) if call.object_id.is_none() => call,
            _ => return Err(Failure::Capability),
        };
        let result = match call.function_name.as_str() {
            "emit" => {
                if call.args.args().len() != 1 || call.args.kwargs().len() != 0 {
                    return Err(Failure::EmitValue);
                }
                let value = call.args.arg(0).ok_or(Failure::EmitValue)?;
                let mut buffer = frame::Buffer::new(emit_bytes);
                write_json(value, &mut buffer, 0)?;
                let bytes = buffer.into_bytes();
                emit_bytes -= bytes.len();
                frame::write(output, frame::EMIT, &bytes)?;
                MontyObject::none()
            }
            "read" | "grep" => {
                if callback_count == CALLBACKS {
                    return Err(Failure::CallbackCount);
                }
                callback_count += 1;
                let (tag, primary) = if call.function_name == "read" {
                    (frame::READ, "path")
                } else {
                    (frame::GREP, "pattern")
                };
                let args = arguments(&call, primary)?;
                frame::write(output, tag, &args)?;
                let (_, bytes) = frame::read(input, |tag| match tag {
                    frame::REPLY => Ok(callback_bytes),
                    _ => Err(Failure::Worker),
                })?;
                callback_bytes -= bytes.len();
                from_json(serde_json::from_slice(&bytes).map_err(|_error| Failure::Worker)?)?
            }
            _ => return Err(Failure::Capability),
        };
        progress = call
            .resume(result, PrintWriter::Disabled)
            .map_err(exception)?;
    }
}

fn exception(error: MontyException) -> Failure {
    match error.exc_type() {
        ExcType::MemoryError => Failure::Memory,
        ExcType::TimeoutError => Failure::GuestTime,
        ExcType::NameError => Failure::Capability,
        _ => Failure::Script,
    }
}

fn arguments(call: &FunctionCall, primary: &str) -> Result<Vec<u8>, Failure> {
    let mut buffer = frame::Buffer::new(ARGUMENT_BYTES);
    if call.args.args().len() > 1 {
        return Err(Failure::Arguments);
    }
    let mut pairs = call.args.kwargs();
    let positional = call.args.arg(0);
    if positional.is_some() && call.args.kwarg(primary).is_some() {
        return Err(Failure::Arguments);
    }
    let result = (|| {
        byte(&mut buffer, b'{')?;
        if let Some(value) = positional {
            write_pair(primary, value, &mut buffer, 1)?;
        }
        let mut separator = positional.is_some();
        for (key, value) in &mut pairs {
            if separator {
                byte(&mut buffer, b',')?;
            }
            write_pair(
                key.as_str().ok_or(Failure::Arguments)?,
                value,
                &mut buffer,
                1,
            )?;
            separator = true;
        }
        byte(&mut buffer, b'}')
    })();
    result.map_err(|_error| Failure::Arguments)?;
    Ok(buffer.into_bytes())
}

fn byte(buffer: &mut frame::Buffer, value: u8) -> Result<(), Failure> {
    buffer
        .write_all(&[value])
        .map_err(|_error| Failure::EmitBytes)
}

fn scalar(buffer: &mut frame::Buffer, value: &impl serde::Serialize) -> Result<(), Failure> {
    serde_json::to_writer(buffer, value).map_err(|_error| Failure::EmitBytes)
}

fn write_pair(
    key: &str,
    value: ObjectRef<'_>,
    buffer: &mut frame::Buffer,
    depth: usize,
) -> Result<(), Failure> {
    scalar(buffer, &key)?;
    byte(buffer, b':')?;
    write_json(value, buffer, depth)
}

fn write_json(
    value: ObjectRef<'_>,
    buffer: &mut frame::Buffer,
    depth: usize,
) -> Result<(), Failure> {
    if depth > 32 {
        return Err(Failure::EmitValue);
    }
    match value.type_name() {
        "NoneType" => scalar(buffer, &()),
        "bool" => scalar(buffer, &value.as_bool().ok_or(Failure::EmitValue)?),
        "int" => scalar(buffer, &value.as_int().ok_or(Failure::EmitValue)?),
        "float" => {
            let number = value.as_float().filter(|number| number.is_finite());
            scalar(buffer, &number.ok_or(Failure::EmitValue)?)
        }
        "str" => scalar(buffer, &value.as_str().ok_or(Failure::EmitValue)?),
        "list" | "tuple" => {
            byte(buffer, b'[')?;
            let values = value
                .items()
                .ok_or(Failure::EmitValue)?
                .into_iter()
                .enumerate();
            for (index, child) in values {
                if index != 0 {
                    byte(buffer, b',')?;
                }
                write_json(child, buffer, depth + 1)?;
            }
            byte(buffer, b']')
        }
        "dict" => {
            byte(buffer, b'{')?;
            let pairs = value
                .pairs()
                .ok_or(Failure::EmitValue)?
                .into_iter()
                .enumerate();
            for (index, (key, child)) in pairs {
                if index != 0 {
                    byte(buffer, b',')?;
                }
                write_pair(
                    key.as_str().ok_or(Failure::EmitValue)?,
                    child,
                    buffer,
                    depth + 1,
                )?;
            }
            byte(buffer, b'}')
        }
        _ => Err(Failure::EmitValue),
    }
}

fn from_json(value: Value) -> Result<MontyObject, Failure> {
    Ok(match value {
        Value::Null => MontyObject::none(),
        Value::Bool(value) => MontyObject::bool(value),
        Value::Number(value) => {
            if let Some(integer) = value.as_i64() {
                MontyObject::int(integer)
            } else if let Some(integer) = value.as_u64() {
                MontyObject::bigint(integer.into())
            } else {
                MontyObject::float(value.as_f64().ok_or(Failure::Worker)?)
            }
        }
        Value::String(value) => MontyObject::string(value),
        Value::Array(values) => {
            let values: Result<Vec<_>, _> = values.into_iter().map(from_json).collect();
            MontyObject::list(values?)
        }
        Value::Object(pairs) => {
            let pairs: Result<Vec<_>, _> = pairs
                .into_iter()
                .map(|(key, value)| Ok((MontyObject::string(key), from_json(value)?)))
                .collect();
            MontyObject::dict(pairs?)
        }
    })
}
