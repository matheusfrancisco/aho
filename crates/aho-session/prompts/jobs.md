detached: Running in the background as job {op}; its outcome arrives later as a "Background job {op}" notice.
interrupted: Interrupted by operator input before it finished.
background: Run in the background as a job: this call returns the job id now and the outcome arrives later as a notice.
suppressed: Its output was suppressed by {cap}.
wake: Background jobs finished while no turn was running; their outcomes follow. Continue the work they belong to, or report what they found.
not_detached: Not run in the background: {why}. It ran as a normal call.
