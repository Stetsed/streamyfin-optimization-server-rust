use anyhow::anyhow;
use rsmpeg::avformat::{AVFormatContextInput, AVFormatContextOutput};
use std::ffi::CStr;

pub fn remux(input_path: &CStr, output_path: &CStr) -> anyhow::Result<()> {
    let mut input_format_context = AVFormatContextInput::open(input_path, None, &mut None)
        .expect("Create input format context failed.");
    input_format_context
        .dump(0, input_path)
        .expect("Dump input format context failed.");
    let mut output_format_context = AVFormatContextOutput::create(output_path, None).unwrap();
    let stream_mapping: Vec<_> = {
        let mut stream_index = 0usize;
        input_format_context
            .streams()
            .iter()
            .map(|stream| {
                let codec_type = stream.codecpar().codec_type();
                if !codec_type.is_video() && !codec_type.is_audio() && !codec_type.is_subtitle() {
                    None
                } else {
                    output_format_context
                        .new_stream()
                        .set_codecpar(stream.codecpar().clone());
                    stream_index += 1;
                    Some(stream_index - 1)
                }
            })
            .collect()
    };
    output_format_context.dump(0, output_path)?;

    output_format_context.write_header(&mut None)?;

    while let Some(mut packet) = input_format_context.read_packet().unwrap() {
        let input_stream_index = packet.stream_index as usize;
        let Some(output_stream_index) = stream_mapping[input_stream_index] else {
            continue;
        };
        {
            let input_stream = &input_format_context.streams()[input_stream_index];
            let output_stream = &output_format_context.streams()[output_stream_index];
            packet.rescale_ts(input_stream.time_base, output_stream.time_base);
            packet.set_stream_index(output_stream_index as i32);
            packet.set_pos(-1);
        }
        output_format_context
            .interleaved_write_frame(&mut packet)
            .unwrap();
    }
    match output_format_context.write_trailer() {
        Ok(_) => Ok(()),
        Err(_) => Err(anyhow!("Get fucked")),
    }
}
