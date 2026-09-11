using System.Diagnostics;

namespace Vorcall.Server.Voice;

// Rate ceiling for one sender, measured in Stopwatch ticks so it is immune to wall clock
// changes. A permit is a packet for the media bucket and a byte for the share budget. Not
// thread-safe; only the relay's receive loop calls it.
public sealed class TokenBucket
{
    private readonly double _permitsPerTick;
    private readonly double _burst;

    private double _tokens;
    private long _lastTicks;

    public TokenBucket(double permitsPerSecond, double burst)
    {
        _permitsPerTick = permitsPerSecond / Stopwatch.Frequency;
        _burst = burst;
        _tokens = burst;
        _lastTicks = Stopwatch.GetTimestamp();
    }

    public bool TryTake(long nowTicks) => TryTake(nowTicks, 1);

    public bool TryTake(long nowTicks, int permits)
    {
        var elapsed = nowTicks - _lastTicks;
        if (elapsed > 0)
        {
            _lastTicks = nowTicks;
            _tokens = Math.Min(_burst, _tokens + (elapsed * _permitsPerTick));
        }

        if (_tokens < permits)
        {
            return false;
        }

        _tokens -= permits;
        return true;
    }
}
