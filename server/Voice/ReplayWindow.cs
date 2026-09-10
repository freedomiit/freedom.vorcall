namespace Vorcall.Server.Voice;

// A 128-packet sliding window over authenticated sequence numbers. _seen is anchored at
// _highest: bit 0 is _highest itself, bit n is _highest - n, so accepting a newer sequence is
// a left shift. Not thread-safe; only the relay's receive loop calls it.
public sealed class ReplayWindow
{
    public const int Size = 128;

    private UInt128 _seen;
    private ulong _highest;

    public bool Accept(ulong seq)
    {
        if (seq > _highest)
        {
            var shift = seq - _highest;
            _seen = shift >= Size ? UInt128.Zero : _seen << (int)shift;
            _seen |= UInt128.One;
            _highest = seq;
            return true;
        }

        var behind = _highest - seq;
        if (behind >= Size)
        {
            return false;
        }

        var mask = UInt128.One << (int)behind;
        if ((_seen & mask) != UInt128.Zero)
        {
            return false;
        }

        _seen |= mask;
        return true;
    }
}
