using Vorcall.Server.Voice;

namespace Vorcall.Server.Tests;

// A voice session's two AEAD calls are a matched pair, and neither may throw: both run on the
// relay's receive loop, and the relay is a BackgroundService whose faulted ExecuteAsync stops the
// whole host — one unforwardable datagram would reset every connected client. A failure is a
// counted drop instead: DropBadTag opening, DropSealFailed sealing.
public class VoiceSessionTests
{
    [Fact]
    public void a_sealed_payload_opens_again_on_the_same_session()
    {
        using var session = NewSession();
        var plaintext = Payload();
        var ciphertext = new byte[plaintext.Length];
        var tag = new byte[MediaHeader.TagLength];

        Assert.True(session.TrySeal(Nonce(), plaintext, ciphertext, tag, Header()));

        var opened = new byte[plaintext.Length];
        Assert.True(session.TryOpen(Nonce(), ciphertext, tag, opened, Header()));
        Assert.Equal(plaintext, opened);
    }

    // The arguments are the ones the round trip seals with, so the disposal is the only thing that
    // can be at fault: a session removed while a datagram was in flight is a drop, not a throw.
    [Fact]
    public void sealing_on_a_disposed_session_answers_false()
    {
        var session = NewSession();
        var plaintext = Payload();
        var ciphertext = new byte[plaintext.Length];
        var tag = new byte[MediaHeader.TagLength];
        session.Dispose();

        Assert.False(session.TrySeal(Nonce(), plaintext, ciphertext, tag, Header()));
    }

    // The payload is one this session had just sealed, so it would open on a live session: the
    // false is the disposal and not a tag that never matched.
    [Fact]
    public void opening_on_a_disposed_session_answers_false()
    {
        var session = NewSession();
        var plaintext = Payload();
        var ciphertext = new byte[plaintext.Length];
        var tag = new byte[MediaHeader.TagLength];
        Assert.True(session.TrySeal(Nonce(), plaintext, ciphertext, tag, Header()));

        session.Dispose();

        Assert.False(session.TryOpen(Nonce(), ciphertext, tag, new byte[plaintext.Length], Header()));
    }

    [Fact]
    public void a_tampered_tag_fails_to_open_instead_of_throwing()
    {
        using var session = NewSession();
        var plaintext = Payload();
        var ciphertext = new byte[plaintext.Length];
        var tag = new byte[MediaHeader.TagLength];
        Assert.True(session.TrySeal(Nonce(), plaintext, ciphertext, tag, Header()));

        tag[0] ^= 0xFF;

        Assert.False(session.TryOpen(Nonce(), ciphertext, tag, new byte[plaintext.Length], Header()));
    }

    // A forged header is rejected the same way, because the header is the associated data.
    [Fact]
    public void a_payload_opened_under_another_header_fails_instead_of_throwing()
    {
        using var session = NewSession();
        var plaintext = Payload();
        var ciphertext = new byte[plaintext.Length];
        var tag = new byte[MediaHeader.TagLength];
        Assert.True(session.TrySeal(Nonce(), plaintext, ciphertext, tag, Header()));

        var forged = Header();
        forged[1] = MediaHeader.TypeVideo;

        Assert.False(session.TryOpen(Nonce(), ciphertext, tag, new byte[plaintext.Length], forged));
    }

    private static VoiceSession NewSession()
        => new(ssrc: 7, userId: 11, channelId: 13, key: new byte[VoiceSession.KeyBytes], shareMaxKbps: 2000);

    private static byte[] Payload() => "twenty milliseconds of Opus"u8.ToArray();

    // As on the wire: the nonce is the header's own ssrc||seq run.
    private static byte[] Nonce() => MediaHeader.NonceOf(Header()).ToArray();

    private static byte[] Header()
    {
        var header = new byte[MediaHeader.Length];
        new MediaHeader(MediaHeader.TypeAudio, MediaHeader.FlagTalkSpurt, Ssrc: 7, Seq: 1, Ts: 960).Write(header);
        return header;
    }
}
