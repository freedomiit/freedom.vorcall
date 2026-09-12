using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class VoiceSignallingTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task JoinVoice_answers_VoiceReady_with_a_unique_ssrc_and_the_VoiceState()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob");
        var (alice, a1) = (party.Account(0), party.Client(0));
        var (bob, b1) = (party.Account(1), party.Client(1));

        // Never asserted or printed whole: VoiceReady carries the media key.
        await a1.SendAsync(Frames.JoinVoice(Session.General));
        var aliceReady = (await a1.ExpectAsync(Kind.VoiceReady)).VoiceReady;
        Assert.Equal(Session.General, aliceReady.RoomId);
        Assert.NotEqual(0u, aliceReady.Ssrc);
        Assert.Equal((uint)Server.VoicePort, aliceReady.Port);
        Assert.Equal("127.0.0.1", aliceReady.Host);
        Assert.Equal(32, aliceReady.Key.Length);
        var aliceState = (await a1.ExpectAsync(Kind.VoiceState)).VoiceState;
        Assert.Equal(Session.General, aliceState.RoomId);
        Assert.Equal(new[] { alice.UserId }, aliceState.Members.Select(member => member.UserId).ToArray());

        await b1.SendAsync(Frames.JoinVoice(Session.General));
        var bobReady = (await b1.ExpectAsync(Kind.VoiceReady)).VoiceReady;
        Assert.NotEqual(0u, bobReady.Ssrc);
        Assert.NotEqual(aliceReady.Ssrc, bobReady.Ssrc);
        var bobState = (await b1.ExpectAsync(Kind.VoiceState)).VoiceState;
        Assert.Equal(
            new[] { alice.UserId, bob.UserId }.Order().ToArray(),
            bobState.Members.Select(member => member.UserId).Order().ToArray());
        Assert.All(bobState.Members, member => Assert.False(member.Sharing));
    }

    [Fact]
    public async Task Voice_joins_and_leaves_are_announced_to_the_whole_text_room()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob");
        var (alice, a1, b1) = (party.Account(0), party.Client(0), party.Client(1));

        await a1.SendAsync(Frames.JoinVoice(Session.General));
        var ready = (await a1.ExpectAsync(Kind.VoiceReady)).VoiceReady;
        await a1.ExpectAsync(Kind.VoiceState);

        // Bob is not in voice and still hears it: the audience is the text room.
        var joined = (await b1.ExpectAsync(Kind.VoiceMemberJoined)).VoiceMemberJoined;
        Assert.Equal(Session.General, joined.RoomId);
        Assert.Equal(alice.UserId, joined.Member.UserId);
        Assert.Equal(alice.Username, joined.Member.Username);
        Assert.Equal(ready.Ssrc, joined.Member.Ssrc);

        await a1.SendAsync(Frames.LeaveVoice(Session.General));
        foreach (var client in new[] { a1, b1 })
        {
            var left = (await client.ExpectAsync(Kind.VoiceMemberLeft)).VoiceMemberLeft;
            Assert.Equal(Session.General, left.RoomId);
            Assert.Equal(alice.UserId, left.UserId);
        }
    }

    [Fact]
    public async Task LeaveVoice_while_not_in_the_channel_answers_NOT_IN_VOICE()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var a1 = await WsClient.ConnectAsync(Server, alice);
        await a1.SendAsync(Frames.LeaveVoice(Session.General));
        await a1.ExpectErrorAsync(ErrorCode.NotInVoice, fatal: false);
    }

    [Fact]
    public async Task JoinVoice_answers_VOICE_UNAVAILABLE_when_the_relay_is_disabled()
    {
        var server = await fixture.VoiceDisabledAsync();
        var alice = await Accounts.RegisterAsync(server, "alice");
        await using var a1 = await WsClient.ConnectAsync(server, alice);
        await a1.SendAsync(Frames.JoinVoice(Session.General));
        await a1.ExpectErrorAsync(ErrorCode.VoiceUnavailable, fatal: false);
    }
}
