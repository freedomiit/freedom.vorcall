namespace Vorcall.Server.Voice;

// A 1024-packet sliding window over authenticated sequence numbers. _seen is anchored at
// _highest: bit 0 of word 0 is _highest itself, bit n of the bitmap is _highest - n, so accepting
// a newer sequence is a left shift of the whole bitmap. A screen share sends far more packets per
// second than voice does, and they share one sequence counter, so the window has to span more
// than a few frames of it. Not thread-safe; only the relay's receive loop calls it.
public sealed class ReplayWindow
{
    public const int Size = 1024;

    private const int Words = Size / 64;

    private readonly ulong[] _seen = new ulong[Words];
    private ulong _highest;

    public bool Accept(ulong seq)
    {
        if (seq > _highest)
        {
            var shift = seq - _highest;
            if (shift >= Size)
            {
                Array.Clear(_seen);
            }
            else
            {
                ShiftLeft((int)shift);
            }

            _seen[0] |= 1UL;
            _highest = seq;
            return true;
        }

        var behind = _highest - seq;
        if (behind >= Size)
        {
            return false;
        }

        var word = (int)(behind >> 6);
        var mask = 1UL << (int)(behind & 63);
        if ((_seen[word] & mask) != 0)
        {
            return false;
        }

        _seen[word] |= mask;
        return true;
    }

    // Shift is always at least one bit, so the word offset and the bit offset are never both zero.
    private void ShiftLeft(int bits)
    {
        var words = bits >> 6;
        var rest = bits & 63;

        if (rest == 0)
        {
            for (var i = Words - 1; i >= words; i--)
            {
                _seen[i] = _seen[i - words];
            }
        }
        else
        {
            for (var i = Words - 1; i > words; i--)
            {
                _seen[i] = (_seen[i - words] << rest) | (_seen[i - words - 1] >> (64 - rest));
            }

            _seen[words] = _seen[0] << rest;
        }

        Array.Clear(_seen, 0, words);
    }
}
