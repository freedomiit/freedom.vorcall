using System.Buffers.Binary;

namespace Vorcall.Server.Voice;

// The 19-byte clear header of a media datagram, big-endian. It is also the AEAD associated
// data, and its ssrc||seq run (bytes 3..15) is the nonce, which is what makes a replayed or
// retargeted header fail to open.
public readonly record struct MediaHeader(byte Type, byte Flags, uint Ssrc, ulong Seq, uint Ts)
{
    public const int Length = 19;
    public const int TagLength = 16;
    public const int MaxDatagram = 512;
    public const int MinDatagram = 35;

    // Set on a pong's seq so it can never share a nonce with the ping it answers.
    public const ulong PongSeqBit = 1UL << 63;

    public const byte TypeAudio = 1;
    public const byte TypePing = 2;
    public const byte TypePong = 3;
    public const byte FlagTalkSpurt = 0x01;

    public const int NonceOffset = 3;
    public const int NonceLength = 12;

    private const byte Version = 1;

    // Inbound only: a client sends audio and pings, never pongs.
    public static bool TryParse(ReadOnlySpan<byte> datagram, out MediaHeader header)
    {
        header = default;
        if (datagram.Length < MinDatagram || datagram[0] != Version)
        {
            return false;
        }

        var type = datagram[1];
        var flags = datagram[2];
        if (type is not (TypeAudio or TypePing) || (flags & ~FlagTalkSpurt) != 0)
        {
            return false;
        }

        header = new MediaHeader(
            type,
            flags,
            BinaryPrimitives.ReadUInt32BigEndian(datagram[3..]),
            BinaryPrimitives.ReadUInt64BigEndian(datagram[7..]),
            BinaryPrimitives.ReadUInt32BigEndian(datagram[15..]));
        return true;
    }

    public static ReadOnlySpan<byte> NonceOf(ReadOnlySpan<byte> datagram) => datagram.Slice(NonceOffset, NonceLength);

    public void Write(Span<byte> destination)
    {
        destination[0] = Version;
        destination[1] = Type;
        destination[2] = Flags;
        BinaryPrimitives.WriteUInt32BigEndian(destination[3..], Ssrc);
        BinaryPrimitives.WriteUInt64BigEndian(destination[7..], Seq);
        BinaryPrimitives.WriteUInt32BigEndian(destination[15..], Ts);
    }
}
