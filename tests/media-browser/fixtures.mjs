import { mkdir } from 'node:fs/promises';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const directory = fileURLToPath(new URL('./artifacts/', import.meta.url));
await mkdir(directory, { recursive: true });
const duration = Number(process.env.MEDIA_FIXTURE_SECONDS ?? 90);
if (!Number.isFinite(duration) || duration < 60) throw new Error('MEDIA_FIXTURE_SECONDS must be at least 60');
const video = process.env.MEDIA_VIDEO_SOURCE ?? 'testsrc2=size=640x360:rate=30';
for (const codec of ['ac3', 'eac3']) {
  for (const container of ['mp4', 'mkv']) {
    const file = `${directory}/${codec}.${container}`;
    const args = ['-y', '-hide_banner', '-loglevel', 'error', '-f', 'lavfi', '-i', video,
      '-f', 'lavfi', '-i', 'aevalsrc=0.2*sin(2*PI*440*t)|0.2*sin(2*PI*880*t)|0.2*sin(2*PI*1760*t)|0|0.2*sin(2*PI*3520*t)|0.2*sin(2*PI*7040*t):s=48000:c=5.1',
      '-t', String(duration), '-map', '0:v', '-map', '1:a', '-c:v', 'libx264', '-preset', 'veryfast',
      '-profile:v', 'main', '-level:v', process.env.MEDIA_H264_LEVEL ?? '3.1', '-g', '30', '-pix_fmt', 'yuv420p', '-c:a', codec, '-b:a', '384k'];
    if (container === 'mp4') args.push('-movflags', '+faststart');
    args.push(file);
    const result = spawnSync(process.env.FFMPEG ?? 'ffmpeg', args, { stdio: 'inherit' });
    if (result.error) throw result.error;
    if (result.status !== 0) throw new Error(`ffmpeg failed for ${file}`);
    console.log(`Generated ${codec}.${container}`);
  }
}
