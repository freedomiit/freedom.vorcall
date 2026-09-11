using System.Buffers.Binary;

namespace Vorcall.Server.Voice;

// The 19-byte clear header of a media datagram, big-endian. It is also the AEAD associated
// data, and its ssrc||seq run (bytes 3..15) is the nonce, which is what makes a replayed or
// retargeted header fail to open.
public readonly record struct MediaHeader(byte Type, byte Flags, uint Ssrc, ulong Seq, uint Ts)
{
    public const int Length = 19;
    public const int TagLength = 16;

    // One datagram fits a 1500-byte path MTU with room to spare for the IPv4 and UDP headers and
    // for a tunnel's own encapsulation.
    public const int MaxDatagram = 1200;
    public const int MinDatagram = 35;

    // Set on a pong's seq so it can never share a nonce with the ping it answers.
    public const ulong PongSeqBit = 1UL << 63;

    public const byte TypeAudio = 1;
    public const byte TypePing = 2;
    public const byte TypePong = 3;
    public const byte TypeVideo = 4;
    public const byte TypeShareAudio = 5;
    public const byte TypeKeyframeRequest = 6;

    // Bit 0 means a talk spurt started on audio, and the first packet of a share-audio run.
    public const byte FlagTalkSpurt = 0x01;

    public const int NonceOffset = 3;
    public const int NonceLength = 12;

    private const byte Version = 1;

    // Inbound only: a client sends audio, pings, share media and keyframe requests, never pongs.
    public static bool TryParse(ReadOnlySpan<byte> datagram, out MediaHeader header)
    {
        header = default;
        if (datagram.Length < MinDatagram || datagram[0] != Version)
        {
            return false;
        }

        var type = datagram[1];
        var flags = datagram[2];
        if (type is not (TypeAudio or TypePing or TypeVideo or TypeShareAudio or TypeKeyframeRequest)
            || (flags & ~FlagTalkSpurt) != 0)
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
