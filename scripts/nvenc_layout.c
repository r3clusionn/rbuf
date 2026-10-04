/* Prints the sizes, field offsets and bitfield positions src/win/nvenc.rs relies on, from NVIDIA's
 * nvEncodeAPI.h (Video Codec SDK 13.x, MIT licensed; not in this repository). The Rust module
 * asserts the same numbers at compile time, so a mistake there fails the build.
 *
 *   cl /nologo /I <folder with nvEncodeAPI.h> scripts\nvenc_layout.c && nvenc_layout.exe
 */
#include <stdio.h>
#include <stddef.h>
#include <string.h>
#include <windows.h>
#include "nvEncodeAPI.h"

#define SIZE(t) printf("size %-40s %5zu\n", #t, sizeof(t))
#define OFF(t, f) printf("off  %-40s %5zu\n", #t "." #f, offsetof(t, f))

/* Byte offset and bit of a one-bit field: set it in a zeroed struct and find the bit. */
#define BIT(t, path)                                                                \
    do {                                                                            \
        static t x;                                                                 \
        memset(&x, 0, sizeof x);                                                    \
        x.path = 1;                                                                 \
        const unsigned char *p = (const unsigned char *)&x;                         \
        for (size_t i = 0; i < sizeof x; i++)                                       \
            if (p[i])                                                               \
                for (int b = 0; b < 8; b++)                                         \
                    if (p[i] >> b & 1)                                              \
                        printf("bit  %-40s %5zu.%d\n", #t "." #path, i, b);         \
    } while (0)

int main(void) {
    printf("api 0x%08x\n", NVENCAPI_VERSION);
    printf("ver NV_ENC_CONFIG_VER 0x%08x\n", NV_ENC_CONFIG_VER);
    printf("ver NV_ENC_INITIALIZE_PARAMS_VER 0x%08x\n", NV_ENC_INITIALIZE_PARAMS_VER);
    printf("ver NV_ENC_PRESET_CONFIG_VER 0x%08x\n", NV_ENC_PRESET_CONFIG_VER);
    printf("ver NV_ENC_PIC_PARAMS_VER 0x%08x\n", NV_ENC_PIC_PARAMS_VER);
    printf("ver NV_ENC_LOCK_BITSTREAM_VER 0x%08x\n", NV_ENC_LOCK_BITSTREAM_VER);
    printf("ver NV_ENC_MAP_INPUT_RESOURCE_VER 0x%08x\n", NV_ENC_MAP_INPUT_RESOURCE_VER);
    printf("ver NV_ENC_REGISTER_RESOURCE_VER 0x%08x\n", NV_ENC_REGISTER_RESOURCE_VER);
    printf("ver NV_ENC_CREATE_BITSTREAM_BUFFER_VER 0x%08x\n", NV_ENC_CREATE_BITSTREAM_BUFFER_VER);
    printf("ver NV_ENC_EVENT_PARAMS_VER 0x%08x\n", NV_ENC_EVENT_PARAMS_VER);
    printf("ver NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER 0x%08x\n", NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER);
    printf("ver NV_ENCODE_API_FUNCTION_LIST_VER 0x%08x\n", NV_ENCODE_API_FUNCTION_LIST_VER);
    printf("ver NV_ENC_RC_PARAMS_VER 0x%08x\n", NV_ENC_RC_PARAMS_VER);

    SIZE(NV_ENCODE_API_FUNCTION_LIST);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncOpenEncodeSessionEx);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodePresetConfigEx);
    OFF(NV_ENCODE_API_FUNCTION_LIST, nvEncLookaheadPicture);

    SIZE(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS);
    OFF(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, apiVersion);
    SIZE(NV_ENC_EVENT_PARAMS);
    SIZE(NV_ENC_CREATE_BITSTREAM_BUFFER);
    OFF(NV_ENC_CREATE_BITSTREAM_BUFFER, bitstreamBuffer);
    SIZE(NV_ENC_REGISTER_RESOURCE);
    OFF(NV_ENC_REGISTER_RESOURCE, resourceToRegister);
    OFF(NV_ENC_REGISTER_RESOURCE, registeredResource);
    OFF(NV_ENC_REGISTER_RESOURCE, bufferFormat);
    OFF(NV_ENC_REGISTER_RESOURCE, pInputFencePoint);
    SIZE(NV_ENC_MAP_INPUT_RESOURCE);
    OFF(NV_ENC_MAP_INPUT_RESOURCE, registeredResource);
    OFF(NV_ENC_MAP_INPUT_RESOURCE, mappedResource);
    OFF(NV_ENC_MAP_INPUT_RESOURCE, mappedBufferFmt);
    SIZE(NV_ENC_LOCK_BITSTREAM);
    OFF(NV_ENC_LOCK_BITSTREAM, outputBitstream);
    OFF(NV_ENC_LOCK_BITSTREAM, bitstreamSizeInBytes);
    OFF(NV_ENC_LOCK_BITSTREAM, outputTimeStamp);
    OFF(NV_ENC_LOCK_BITSTREAM, bitstreamBufferPtr);
    OFF(NV_ENC_LOCK_BITSTREAM, pictureType);
    OFF(NV_ENC_LOCK_BITSTREAM, frameIdxDisplay);
    SIZE(NV_ENC_PIC_PARAMS);
    OFF(NV_ENC_PIC_PARAMS, inputTimeStamp);
    OFF(NV_ENC_PIC_PARAMS, inputBuffer);
    OFF(NV_ENC_PIC_PARAMS, outputBitstream);
    OFF(NV_ENC_PIC_PARAMS, completionEvent);
    OFF(NV_ENC_PIC_PARAMS, bufferFmt);
    OFF(NV_ENC_PIC_PARAMS, pictureStruct);
    OFF(NV_ENC_PIC_PARAMS, codecPicParams);
    SIZE(NV_ENC_CODEC_PIC_PARAMS);

    SIZE(NV_ENC_INITIALIZE_PARAMS);
    OFF(NV_ENC_INITIALIZE_PARAMS, encodeGUID);
    OFF(NV_ENC_INITIALIZE_PARAMS, presetGUID);
    OFF(NV_ENC_INITIALIZE_PARAMS, encodeWidth);
    OFF(NV_ENC_INITIALIZE_PARAMS, frameRateNum);
    OFF(NV_ENC_INITIALIZE_PARAMS, enableEncodeAsync);
    OFF(NV_ENC_INITIALIZE_PARAMS, enablePTD);
    OFF(NV_ENC_INITIALIZE_PARAMS, privDataSize);
    OFF(NV_ENC_INITIALIZE_PARAMS, encodeConfig);
    OFF(NV_ENC_INITIALIZE_PARAMS, maxEncodeWidth);
    OFF(NV_ENC_INITIALIZE_PARAMS, tuningInfo);
    OFF(NV_ENC_INITIALIZE_PARAMS, bufferFormat);
    SIZE(NV_ENC_PRESET_CONFIG);
    OFF(NV_ENC_PRESET_CONFIG, presetCfg);

    SIZE(NV_ENC_CONFIG);
    OFF(NV_ENC_CONFIG, profileGUID);
    OFF(NV_ENC_CONFIG, gopLength);
    OFF(NV_ENC_CONFIG, frameIntervalP);
    OFF(NV_ENC_CONFIG, rcParams);
    OFF(NV_ENC_CONFIG, encodeCodecConfig);
    SIZE(NV_ENC_CODEC_CONFIG);
    SIZE(NV_ENC_RC_PARAMS);
    OFF(NV_ENC_RC_PARAMS, rateControlMode);
    OFF(NV_ENC_RC_PARAMS, constQP);
    OFF(NV_ENC_RC_PARAMS, averageBitRate);
    OFF(NV_ENC_RC_PARAMS, maxBitRate);
    OFF(NV_ENC_RC_PARAMS, vbvBufferSize);
    OFF(NV_ENC_RC_PARAMS, vbvInitialDelay);
    BIT(NV_ENC_RC_PARAMS, enableAQ);
    BIT(NV_ENC_RC_PARAMS, zeroReorderDelay);
    OFF(NV_ENC_RC_PARAMS, targetQuality);
    OFF(NV_ENC_RC_PARAMS, multiPass);

    BIT(NV_ENC_CONFIG_H264, repeatSPSPPS);
    BIT(NV_ENC_CONFIG_H264, outputAUD);
    OFF(NV_ENC_CONFIG_H264, idrPeriod);
    OFF(NV_ENC_CONFIG_H264, h264VUIParameters);
    OFF(NV_ENC_CONFIG_H264, chromaFormatIDC);
    BIT(NV_ENC_CONFIG_HEVC, repeatSPSPPS);
    BIT(NV_ENC_CONFIG_HEVC, outputAUD);
    OFF(NV_ENC_CONFIG_HEVC, idrPeriod);
    OFF(NV_ENC_CONFIG_HEVC, hevcVUIParameters);
    BIT(NV_ENC_CONFIG_AV1, outputAnnexBFormat);
    BIT(NV_ENC_CONFIG_AV1, repeatSeqHdr);
    OFF(NV_ENC_CONFIG_AV1, idrPeriod);
    OFF(NV_ENC_CONFIG_AV1, colorPrimaries);
    OFF(NV_ENC_CONFIG_AV1, transferCharacteristics);
    OFF(NV_ENC_CONFIG_AV1, matrixCoefficients);
    OFF(NV_ENC_CONFIG_AV1, colorRange);
    SIZE(NV_ENC_CONFIG_H264_VUI_PARAMETERS);
    OFF(NV_ENC_CONFIG_H264_VUI_PARAMETERS, videoSignalTypePresentFlag);
    OFF(NV_ENC_CONFIG_H264_VUI_PARAMETERS, videoFormat);
    OFF(NV_ENC_CONFIG_H264_VUI_PARAMETERS, videoFullRangeFlag);
    OFF(NV_ENC_CONFIG_H264_VUI_PARAMETERS, colourDescriptionPresentFlag);
    OFF(NV_ENC_CONFIG_H264_VUI_PARAMETERS, colourPrimaries);
    OFF(NV_ENC_CONFIG_H264_VUI_PARAMETERS, transferCharacteristics);
    OFF(NV_ENC_CONFIG_H264_VUI_PARAMETERS, colourMatrix);
    printf("enum NV_ENC_BUFFER_FORMAT_NV12 0x%x ARGB 0x%x ABGR 0x%x\n", NV_ENC_BUFFER_FORMAT_NV12, NV_ENC_BUFFER_FORMAT_ARGB,
           NV_ENC_BUFFER_FORMAT_ABGR);
    printf("enum NV_ENC_PIC_FLAG_FORCEIDR 0x%x OUTPUT_SPSPPS 0x%x EOS 0x%x\n", NV_ENC_PIC_FLAG_FORCEIDR, NV_ENC_PIC_FLAG_OUTPUT_SPSPPS,
           NV_ENC_PIC_FLAG_EOS);
    printf("enum RC CONSTQP %d VBR %d CBR %d\n", NV_ENC_PARAMS_RC_CONSTQP, NV_ENC_PARAMS_RC_VBR, NV_ENC_PARAMS_RC_CBR);
    printf("enum PIC_TYPE IDR %d I %d P %d\n", NV_ENC_PIC_TYPE_IDR, NV_ENC_PIC_TYPE_I, NV_ENC_PIC_TYPE_P);
    printf("enum DEVICE DIRECTX %d; RESOURCE DIRECTX %d; USAGE INPUT_IMAGE %d\n", NV_ENC_DEVICE_TYPE_DIRECTX,
           NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX, NV_ENC_INPUT_IMAGE);
    printf("enum ERR NEED_MORE_INPUT %d LOCK_BUSY %d ENCODER_BUSY %d\n", NV_ENC_ERR_NEED_MORE_INPUT, NV_ENC_ERR_LOCK_BUSY,
           NV_ENC_ERR_ENCODER_BUSY);
    printf("enum MULTIPASS DISABLED %d; VUI 709 prim %d trc %d mat %d; 601 mat %d; FORMAT UNSPEC %d\n", NV_ENC_MULTI_PASS_DISABLED,
           NV_ENC_VUI_COLOR_PRIMARIES_BT709, NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709, NV_ENC_VUI_MATRIX_COEFFS_BT709,
           NV_ENC_VUI_MATRIX_COEFFS_SMPTE170M, NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED);
    return 0;
}
