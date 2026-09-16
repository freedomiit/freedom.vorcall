namespace Vorcall.Server.Metrics;

// The counters a scrape reads, and nothing else: one interlocked add on a hot path's way out,
// no allocation, no lock, no dictionary to grow. Everything that varies per room or per account
// is deliberately absent — a label with unbounded cardinality is how a metrics endpoint becomes
// the thing that takes the server down.
public sealed class ServerMetrics
{
    private long _messagesTotal;
    private long _uploadsTotal;
    private long _imageUploadsTotal;
    private long _soundUploadsTotal;
    private long _stickerUploadsTotal;
    private long _http2xx;
    private long _http4xx;
    private long _http5xx;
    private long _httpRateLimitedTotal;
    private long _wsAcceptedTotal;

    public DateTime StartedAt { get; } = DateTime.UtcNow;

    public long MessagesTotal => Interlocked.Read(ref _messagesTotal);

    public long UploadsTotal => Interlocked.Read(ref _uploadsTotal);

    public long ImageUploadsTotal => Interlocked.Read(ref _imageUploadsTotal);

    public long SoundUploadsTotal => Interlocked.Read(ref _soundUploadsTotal);

    public long StickerUploadsTotal => Interlocked.Read(ref _stickerUploadsTotal);

    public long Http2xx => Interlocked.Read(ref _http2xx);

    public long Http4xx => Interlocked.Read(ref _http4xx);

    public long Http5xx => Interlocked.Read(ref _http5xx);

    public long HttpRateLimitedTotal => Interlocked.Read(ref _httpRateLimitedTotal);

    public long WsAcceptedTotal => Interlocked.Read(ref _wsAcceptedTotal);

    public void CountMessage() => Interlocked.Increment(ref _messagesTotal);

    public void CountUpload() => Interlocked.Increment(ref _uploadsTotal);

    public void CountImageUpload() => Interlocked.Increment(ref _imageUploadsTotal);

    public void CountSoundUpload() => Interlocked.Increment(ref _soundUploadsTotal);

    public void CountStickerUpload() => Interlocked.Increment(ref _stickerUploadsTotal);

    // Only the three classes the exposition carries: a 101 upgrade and a redirect belong to none
    // of them and are counted nowhere rather than folded into a class they are not.
    public void CountResponse(int status)
    {
        switch (status)
        {
            case >= 200 and < 300:
                Interlocked.Increment(ref _http2xx);
                break;
            case >= 400 and < 500:
                Interlocked.Increment(ref _http4xx);
                break;
            case >= 500:
                Interlocked.Increment(ref _http5xx);
                break;
        }
    }

    public void CountHttpRateLimited() => Interlocked.Increment(ref _httpRateLimitedTotal);

    public void CountWsAccepted() => Interlocked.Increment(ref _wsAcceptedTotal);
}
