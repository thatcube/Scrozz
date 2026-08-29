//! AVCaptureSession microphone fallback for macOS 14.

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_av_foundation::{
    AVCaptureAudioDataOutput, AVCaptureAudioDataOutputSampleBufferDelegate, AVCaptureDevice,
    AVCaptureDeviceInput, AVCaptureSession, AVMediaTypeAudio,
};
use scrozz_core::{Error, Result};

pub(crate) struct MicrophoneCapture {
    session: Retained<AVCaptureSession>,
    output: Retained<AVCaptureAudioDataOutput>,
}

// SAFETY: the capture objects are configured before this value crosses a
// thread. Thereafter only AVFoundation's synchronized start/stop and delegate
// clearing methods are used.
unsafe impl Send for MicrophoneCapture {}

impl MicrophoneCapture {
    pub(crate) fn start(
        delegate: &ProtocolObject<dyn AVCaptureAudioDataOutputSampleBufferDelegate>,
        queue: &DispatchQueue,
    ) -> Result<Self> {
        // SAFETY: immutable weak-linked AVFoundation constant.
        let media_type = unsafe { AVMediaTypeAudio }.ok_or_else(|| Error::Unsupported {
            what: "microphone recording".to_owned(),
            why: "AVFoundation did not expose the audio media type".to_owned(),
        })?;
        // SAFETY: queries the current default audio capture device.
        let device = unsafe { AVCaptureDevice::defaultDeviceWithMediaType(media_type) }
            .ok_or_else(|| Error::Unsupported {
                what: "microphone recording".to_owned(),
                why: "no audio input device is available".to_owned(),
            })?;
        // SAFETY: opens the selected audio device for this capture session.
        let input = unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&device) }.map_err(
            |failure| Error::Platform(super::error::describe(&failure, "opening the microphone")),
        )?;
        // SAFETY: ordinary AVFoundation object construction.
        let session = unsafe { AVCaptureSession::new() };
        // SAFETY: ordinary AVFoundation object construction.
        let output = unsafe { AVCaptureAudioDataOutput::new() };

        // SAFETY: all mutations occur within one configuration transaction and
        // are guarded by canAddInput/canAddOutput.
        unsafe {
            session.beginConfiguration();
            if !session.canAddInput(&input) {
                session.commitConfiguration();
                return Err(Error::Platform(
                    "AVCaptureSession refused the microphone input".to_owned(),
                ));
            }
            session.addInput(&input);
            if !session.canAddOutput(&output) {
                session.commitConfiguration();
                return Err(Error::Platform(
                    "AVCaptureSession refused the microphone audio output".to_owned(),
                ));
            }
            session.addOutput(&output);
            output.setSampleBufferDelegate_queue(Some(delegate), Some(queue));
            session.commitConfiguration();
            session.startRunning();
        }

        Ok(Self { session, output })
    }

    pub(crate) fn stop(&self) {
        // SAFETY: clearing the delegate prevents callbacks after stop returns.
        unsafe {
            self.output.setSampleBufferDelegate_queue(None, None);
            self.session.stopRunning();
        }
    }
}
