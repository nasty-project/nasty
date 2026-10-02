import assert from 'node:assert/strict';

export function assertSteady(baseline, last, allowVideoBuffering = false) {
  assert.equal(last.pageErrors.length, 0, 'Page error during playback');
  assert.equal(last.video.errorCode, null, 'Native video decoder error');
  if (allowVideoBuffering && last.diagnostics.audio.status === 'buffering') return;
  assert(last.video.time > baseline.video.time + 0.05, 'Video did not advance');
  assert.equal(last.diagnostics.audio.status, 'playing');
  for (const field of ['lateBatches', 'droppedBatches', 'possibleQueueGaps', 'sourceDiscontinuities']) {
    assert.equal(last.diagnostics.audio[field], baseline.diagnostics.audio[field], `${field} increased during steady playback`);
  }
  const waits = report => report.events.filter(event => event.type === 'waiting' && event.readyState < 3).length;
  const allowedRestarts = allowVideoBuffering ? waits(last) - waits(baseline) : 0;
  assert(last.diagnostics.audio.decoderStarts <= baseline.diagnostics.audio.decoderStarts + allowedRestarts,
    'Decoder restarted without an expected native video waiting event');
  assert.equal(last.diagnostics.audio.lookAheadTargetMs, 1000);
  assert(last.diagnostics.audio.queuedAheadMs > 0, 'Audio queue drained');
}
