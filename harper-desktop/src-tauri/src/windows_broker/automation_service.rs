use std::iter::once;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};

use crate::rect::Rect;
use crate::windows_broker::get_focused_monitor_scale;
use harper_core::{Span, linting::Suggestion};
use is_macro::Is;
use uiautomation::types::{
    ControlType, Handle, TextPatternRangeEndpoint, TextUnit, TreeScope, UIProperty,
};
use uiautomation::variants::Variant;
use uiautomation::{
    UIAutomation, UIElement,
    core::UICondition,
    patterns::{UITextPattern, UIValuePattern},
};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Accessibility::IUIAutomationTextRange;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

/// Information about a worker thread.
struct WorkerData {
    sender: SyncSender<(WorkerJob, Vec<JobArgument>)>,
    receiver: Receiver<JobResult>,
}

#[derive(Debug)]
struct ApplySuggestionRequest {
    window: isize,
    expected_text: String,
    span: Span<char>,
    suggestion: Suggestion,
}

#[derive(Debug, Is)]
enum JobArgument {
    Span(Span<char>),
    Window(isize),
    Text(String),
    ApplySuggestion(ApplySuggestionRequest),
}

/// The result of a job run by the worker thread.
#[derive(Debug, Is)]
enum JobResult {
    None,
    String(String),
    GroupedRects(Vec<Vec<Rect>>),
    Err,
}

/// An actual function pointer to be run by the worker thread.
type WorkerJob = fn(&UIAutomation, Vec<JobArgument>) -> JobResult;

/// A job whose result has not been collected yet.
#[derive(Debug, Clone, Copy)]
enum InFlight {
    Text,
    Rects,
    /// The caller lost interest, for example because the focused window changed. The result is
    /// thrown away when it arrives.
    Discarded,
}

/// Progress of a non-blocking accessibility read.
#[derive(Debug)]
pub enum ReadStep {
    /// No job is running, so a new one may be started.
    Idle,
    /// The worker is still busy; there is nothing to do this frame.
    Pending,
    /// The focused element's text.
    Text(String),
    /// One group of bounding boxes per requested span, in request order.
    Rects(Vec<Vec<Rect>>),
    /// The job failed or produced an unexpected result.
    Failed,
}

/// Runs and communicates with a worker thread to interact with the Win32 Automation API to query the accessibility tree.
/// Necessary because the API has very specific thread setting requirements to work.
///
/// Reads are non-blocking: the caller starts a job with `request_text` or `request_rects` and
/// collects it later with `poll`, so an accessibility provider that answers slowly, as Chromium
/// does while it is busy, never stalls the overlay's event loop. Applying a suggestion is a user
/// action and still waits for its result.
pub struct AutomationService {
    worker_data: Option<WorkerData>,
    // Needed to redirect focus to the last focused window when the focus arrives on the Harper highlighter window
    last_focused_window: Option<isize>,
    /// The job currently running on the worker, if any.
    in_flight: Option<InFlight>,
}

impl AutomationService {
    pub fn create_and_start() -> Self {
        let mut output = Self {
            last_focused_window: None,
            worker_data: None,
            in_flight: None,
        };

        output.start_worker_thread();

        output
    }

    /// Starts the worker thread if it is not already running.
    /// Does nothing if the worker thread is already running.
    fn start_worker_thread(&mut self) {
        let (job_sender, job_receiver) = sync_channel::<(WorkerJob, Vec<JobArgument>)>(1);
        let (result_sender, result_receiver) = sync_channel(1);

        std::thread::spawn(move || {
            let automation = UIAutomation::new().unwrap();

            // `recv` sleeps until a job arrives and fails once the service, and with it the
            // sending side of the channel, has been dropped.
            while let Ok((job, arguments)) = job_receiver.recv() {
                let result = job(&automation, arguments);

                // Stop the thread if the other side of the channel has been closed (or dropped).
                if result_sender.send(result).is_err() {
                    break;
                }
            }
        });

        self.worker_data = Some(WorkerData {
            receiver: result_receiver,
            sender: job_sender,
        });
    }

    /// Stops the worker thread if it is running. This method does nothing if it is not running.
    fn stop_worker_thread(&mut self) {
        // This drops the inner fields, which closes the channel, which signals to the worker to stop running.
        self.worker_data = None;
    }

    /// Runs a worker job and waits for its result. Returns `None` if the worker thread does not
    /// exist. A job still in flight is waited for and discarded first so results stay in order.
    fn run_worker_job(&mut self, job: WorkerJob, arguments: Vec<JobArgument>) -> Option<JobResult> {
        let worker_data = self.worker_data.as_ref()?;
        if self.in_flight.take().is_some() {
            let _ = worker_data.receiver.recv();
        }
        worker_data.sender.send((job, arguments)).ok()?;
        worker_data.receiver.recv().ok()
    }

    /// Starts a job without waiting for it. Returns `false` if the worker is missing or busy.
    fn start_job(&mut self, kind: InFlight, job: WorkerJob, arguments: Vec<JobArgument>) -> bool {
        if self.in_flight.is_some() {
            return false;
        }
        let Some(worker_data) = self.worker_data.as_ref() else {
            return false;
        };
        if worker_data.sender.try_send((job, arguments)).is_err() {
            return false;
        }
        self.in_flight = Some(kind);
        true
    }

    /// Starts reading the text of the focused element in the foreground window. Collect the
    /// result with `poll`.
    pub fn request_text(&mut self) -> bool {
        let Some(window) = self.resolve_focused_window() else {
            return false;
        };
        self.start_job(
            InFlight::Text,
            get_text_job,
            vec![JobArgument::Window(window)],
        )
    }

    /// Collects the running job's result if it has finished, without waiting.
    pub fn poll(&mut self) -> ReadStep {
        let Some(kind) = self.in_flight else {
            return ReadStep::Idle;
        };
        let Some(worker_data) = self.worker_data.as_ref() else {
            self.in_flight = None;
            return ReadStep::Failed;
        };

        match worker_data.receiver.try_recv() {
            Err(TryRecvError::Empty) => ReadStep::Pending,
            Err(TryRecvError::Disconnected) => {
                self.in_flight = None;
                ReadStep::Failed
            }
            Ok(result) => {
                self.in_flight = None;
                match (kind, result) {
                    (InFlight::Discarded, _) => ReadStep::Idle,
                    (InFlight::Text, JobResult::String(text)) => ReadStep::Text(text),
                    (InFlight::Rects, JobResult::GroupedRects(rects)) => ReadStep::Rects(rects),
                    _ => ReadStep::Failed,
                }
            }
        }
    }

    /// Marks the running job's result as unwanted, for example after the focused window changed.
    pub fn discard_pending(&mut self) {
        if self.in_flight.is_some() {
            self.in_flight = Some(InFlight::Discarded);
        }
    }

    pub fn apply_suggestion(
        &mut self,
        expected_text: String,
        span: Span<char>,
        suggestion: Suggestion,
    ) {
        let Some(window) = self.resolve_focused_window() else {
            return;
        };

        let request = ApplySuggestionRequest {
            window,
            expected_text,
            span,
            suggestion,
        };

        let _ = self.run_worker_job(
            apply_suggestion_job,
            vec![JobArgument::ApplySuggestion(request)],
        );
    }

    /// Starts computing the bounding boxes of `spans` within the focused element holding `text`.
    /// Each span may have multiple bounding boxes, and the result keeps the input order. Collect
    /// the result with `poll`.
    pub fn request_rects(
        &mut self,
        text: &str,
        spans: impl IntoIterator<Item = Span<char>>,
    ) -> bool {
        let Some(window) = self.resolve_focused_window() else {
            return false;
        };
        self.start_job(
            InFlight::Rects,
            get_bounding_rect_job,
            once(JobArgument::Window(window))
                .chain(once(JobArgument::Text(text.to_string())))
                .chain(spans.into_iter().map(JobArgument::Span))
                .collect(),
        )
    }

    /// Returns the foreground source window, retaining the last external window while Harper's
    /// overlay owns focus.
    pub fn resolve_focused_window(&mut self) -> Option<isize> {
        let (focused_window, focused_process_id) = focused_window()?;

        if focused_process_id == std::process::id() {
            return self.last_focused_window;
        }

        self.last_focused_window = Some(focused_window);
        Some(focused_window)
    }
}

impl Drop for AutomationService {
    fn drop(&mut self) {
        self.stop_worker_thread();
    }
}

fn apply_suggestion_job(automation: &UIAutomation, mut arguments: Vec<JobArgument>) -> JobResult {
    let Some(JobArgument::ApplySuggestion(request)) = arguments.pop() else {
        return JobResult::Err;
    };

    if !arguments.is_empty() {
        return JobResult::Err;
    }

    let Ok(element) =
        text_element_for_window(automation, request.window, Some(&request.expected_text))
    else {
        eprintln!(
            "Unable to apply Windows suggestion: the source text element is no longer available"
        );
        return JobResult::None;
    };

    let Ok(current_text) = get_text(&element) else {
        eprintln!("Unable to apply Windows suggestion: the source text can no longer be read");
        return JobResult::None;
    };

    let updated_text = match apply_suggestion_to_text(
        &current_text,
        &request.expected_text,
        request.span,
        &request.suggestion,
    ) {
        Ok(updated_text) => updated_text,
        Err(error) => {
            eprintln!("Unable to apply Windows suggestion: {error}");
            return JobResult::None;
        }
    };

    let Ok(value_pattern) = element.get_pattern::<UIValuePattern>() else {
        eprintln!(
            "Unable to apply Windows suggestion: the text element has no writable value pattern"
        );
        return JobResult::None;
    };

    match value_pattern.is_readonly() {
        Ok(true) => {
            eprintln!("Unable to apply Windows suggestion: the text element is read-only");
        }
        Ok(false) => {
            if let Err(error) = value_pattern.set_value(&updated_text) {
                eprintln!("Unable to apply Windows suggestion: {error}");
            }
        }
        Err(error) => {
            eprintln!("Unable to determine whether the Windows text element is writable: {error}");
        }
    }

    JobResult::None
}

fn apply_suggestion_to_text(
    current_text: &str,
    expected_text: &str,
    span: Span<char>,
    suggestion: &Suggestion,
) -> std::result::Result<String, &'static str> {
    if current_text != expected_text {
        return Err("the source text changed after linting");
    }

    let mut chars = current_text.chars().collect::<Vec<_>>();
    if span.end > chars.len() {
        return Err("the lint span is outside the source text");
    }

    suggestion.apply(span, &mut chars);
    Ok(chars.into_iter().collect())
}

fn get_text(element: &UIElement) -> uiautomation::Result<String> {
    let pattern: UITextPattern = element.get_pattern()?;
    let range = pattern.get_document_range()?;
    range.get_text(-1)
}

/// Matches text providers the user can type into.
///
/// Text that is only displayed, such as a web page or a chat transcript, also exposes the text
/// pattern and can hold keyboard focus, which made Harper lint whole pages. Everything the user
/// can edit reports a value that is not read-only, so elements with a read-only value and plain
/// text labels are excluded.
fn editable_text_condition(automation: &UIAutomation) -> uiautomation::Result<UICondition> {
    let property = |property: UIProperty, value: Variant| {
        automation.create_property_condition(property, value, None)
    };

    let has_text = property(UIProperty::IsTextPatternAvailable, Variant::from(true))?;
    let read_only_value = automation.create_and_condition(
        property(UIProperty::IsValuePatternAvailable, Variant::from(true))?,
        property(UIProperty::ValueIsReadOnly, Variant::from(true))?,
    )?;
    let label = property(
        UIProperty::ControlType,
        Variant::from(ControlType::Text as i32),
    )?;
    let editable = automation.create_and_condition(
        automation.create_not_condition(read_only_value)?,
        automation.create_not_condition(label)?,
    )?;

    automation.create_and_condition(has_text, editable)
}

/// Finds the focused editable text element below `window`.
///
/// Chromium-based apps, including Electron apps, report keyboard focus on both the focused text
/// field and the document that contains it, and a subtree search lists the container first. The
/// element the system itself reports as focused is preferred, falling back to the last (deepest)
/// match, so the text field wins over its container.
///
/// When `expected_text` is provided, unrelated text providers are excluded. Clicking Harper's own
/// popup can take keyboard focus away from the source element before a suggestion is applied, so
/// the search then also accepts an unfocused element that holds exactly the expected text.
fn text_element_for_window(
    automation: &UIAutomation,
    window: isize,
    expected_text: Option<&str>,
) -> uiautomation::Result<UIElement> {
    let root = automation.element_from_handle(Handle::from(window))?;
    let keyboard_condition = automation.create_property_condition(
        UIProperty::HasKeyboardFocus,
        Variant::from(true),
        None,
    )?;
    let focused_condition = automation
        .create_and_condition(editable_text_condition(automation)?, keyboard_condition)?;
    let focused_elements = root.find_all(TreeScope::Subtree, &focused_condition)?;

    let Some(expected) = expected_text else {
        let system_focused = automation.get_focused_element().ok();
        let is_system_focused = |element: &UIElement| {
            system_focused.as_ref().is_some_and(|focused| {
                automation
                    .compare_elements(element, focused)
                    .unwrap_or(false)
            })
        };

        return focused_elements
            .iter()
            .find(|element| is_system_focused(element))
            .or(focused_elements.last())
            .cloned()
            .ok_or_else(no_text_element);
    };

    let holds_expected_text =
        |element: &UIElement| get_text(element).is_ok_and(|text| text == expected);

    if let Some(element) = focused_elements
        .iter()
        .find(|element| holds_expected_text(element))
    {
        return Ok(element.clone());
    }

    root.find_all(TreeScope::Subtree, &editable_text_condition(automation)?)?
        .into_iter()
        .find(holds_expected_text)
        .ok_or_else(no_text_element)
}

fn no_text_element() -> Error {
    Error::new(uiautomation::errors::ERR_NOTFOUND, "no text element found")
}

fn get_text_job(automation: &UIAutomation, args: Vec<JobArgument>) -> JobResult {
    let Some(JobArgument::Window(window)) = args.first() else {
        return JobResult::Err;
    };
    let Ok(element) = text_element_for_window(automation, *window, None) else {
        return JobResult::Err;
    };

    get_text(&element).map_or(JobResult::Err, JobResult::String)
}

use std::{ffi::c_void, mem::size_of};

use uiautomation::{Error, Result};
use windows::Win32::System::{
    Com::SAFEARRAY,
    Ole::{
        SafeArrayDestroy, SafeArrayGetDim, SafeArrayGetElement, SafeArrayGetElemsize,
        SafeArrayGetLBound, SafeArrayGetUBound,
    },
};

struct OwnedSafeArray(*mut SAFEARRAY);

impl Drop for OwnedSafeArray {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = SafeArrayDestroy(self.0);
            }
        }
    }
}

fn bounding_rectangles_for_span(
    element: &UIElement,
    start: i32,
    len: i32,
) -> Result<Vec<(f64, f64, f64, f64)>> {
    if start < 0 || len < 0 {
        return Err(Error::new(
            uiautomation::errors::ERR_INVALID_ARG,
            "start and len must be non-negative",
        ));
    }

    let pattern: UITextPattern = element.get_pattern()?;
    let range = pattern.get_document_range()?;

    range.move_endpoint_by_range(
        TextPatternRangeEndpoint::End,
        &range,
        TextPatternRangeEndpoint::Start,
    )?;

    range.move_endpoint_by_unit(TextPatternRangeEndpoint::Start, TextUnit::Character, start)?;

    range.move_endpoint_by_range(
        TextPatternRangeEndpoint::End,
        &range,
        TextPatternRangeEndpoint::Start,
    )?;

    range.move_endpoint_by_unit(TextPatternRangeEndpoint::End, TextUnit::Character, len)?;

    let raw: &IUIAutomationTextRange = range.as_ref();
    let array = OwnedSafeArray(unsafe { raw.GetBoundingRectangles()? });

    if array.0.is_null() {
        return Ok(Vec::new());
    }

    let dim = unsafe { SafeArrayGetDim(array.0) };

    if dim != 1 {
        return Err(Error::new(
            uiautomation::errors::ERR_FORMAT,
            "bounding rectangles SAFEARRAY is not one-dimensional",
        ));
    }

    let elem_size = unsafe { SafeArrayGetElemsize(array.0) };

    if elem_size as usize != size_of::<f64>() {
        return Err(Error::new(
            uiautomation::errors::ERR_FORMAT,
            "bounding rectangles SAFEARRAY does not contain f64-sized elements",
        ));
    }

    let lower = unsafe { SafeArrayGetLBound(array.0, 1)? };
    let upper = unsafe { SafeArrayGetUBound(array.0, 1)? };

    if upper < lower {
        return Ok(Vec::new());
    }

    let count = usize::try_from(i64::from(upper) - i64::from(lower) + 1)
        .map_err(|_| Error::new(uiautomation::errors::ERR_FORMAT, "SAFEARRAY is too large"))?;

    if count % 4 != 0 {
        return Err(Error::new(
            uiautomation::errors::ERR_FORMAT,
            "bounding rectangles SAFEARRAY length is not divisible by four",
        ));
    }

    let mut result = Vec::with_capacity(count / 4);

    for rect in 0..count / 4 {
        let mut values = [0.0_f64; 4];

        for (component, value) in values.iter_mut().enumerate() {
            let offset = rect
                .checked_mul(4)
                .and_then(|n| n.checked_add(component))
                .ok_or_else(|| {
                    Error::new(uiautomation::errors::ERR_FORMAT, "SAFEARRAY index overflow")
                })?;

            let index = i64::from(lower)
                .checked_add(i64::try_from(offset).map_err(|_| {
                    Error::new(uiautomation::errors::ERR_FORMAT, "SAFEARRAY index overflow")
                })?)
                .and_then(|n| i32::try_from(n).ok())
                .ok_or_else(|| {
                    Error::new(uiautomation::errors::ERR_FORMAT, "SAFEARRAY index overflow")
                })?;

            unsafe {
                SafeArrayGetElement(array.0, &index, value as *mut f64 as *mut c_void)?;
            }
        }

        result.push((values[0], values[1], values[2], values[3]));
    }

    Ok(result)
}

fn get_bounding_rect_job(automation: &UIAutomation, arguments: Vec<JobArgument>) -> JobResult {
    let Some(JobArgument::Window(window)) = arguments.first() else {
        return JobResult::Err;
    };
    let Some(JobArgument::Text(expected_text)) = arguments.get(1) else {
        return JobResult::Err;
    };
    let Ok(text_element) = text_element_for_window(automation, *window, Some(expected_text)) else {
        return JobResult::Err;
    };

    let effective_monitor_scale = get_focused_monitor_scale();

    let mut rects = Vec::with_capacity(arguments.len().saturating_sub(2));

    for span in arguments.into_iter().skip(2) {
        let span = span.expect_span();

        let Ok(found_rects) =
            bounding_rectangles_for_span(&text_element, span.start as i32, span.len() as i32)
        else {
            return JobResult::Err;
        };

        rects.push(
            found_rects
                .iter()
                .map(|(x, y, w, h)| {
                    Rect::new(
                        *x / effective_monitor_scale,
                        *y / effective_monitor_scale,
                        *w / effective_monitor_scale,
                        *h / effective_monitor_scale,
                    )
                })
                .collect(),
        );
    }

    JobResult::GroupedRects(rects)
}

fn focused_window() -> Option<(isize, u32)> {
    let hwnd: HWND = unsafe { GetForegroundWindow() };

    if hwnd.0.is_null() {
        return None;
    }

    let mut process_id = 0;

    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    }

    Some((hwnd.0 as isize, process_id))
}
