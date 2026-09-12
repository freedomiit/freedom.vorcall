using Vorcall.Server.Permissions;
using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

// PROTOCOL.md § Voice: the signalling only, with no media on the wire. Voice lives on a voice
// channel, never on a text one, so the subject throughout is the voice channel the seed puts
// beside general, or a voice channel the owner creates for the test.
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
        var voiceId = party.GeneralVoiceId;

        // Never asserted or printed whole: VoiceReady carries the media key.
        await a1.SendAsync(Frames.JoinVoice(voiceId));
        var aliceReady = (await a1.ExpectAsync(Kind.VoiceReady)).VoiceReady;
        Assert.Equal(voiceId, aliceReady.ChannelId);
        Assert.NotEqual(0u, aliceReady.Ssrc);
        Assert.Equal((uint)Server.VoicePort, aliceReady.Port);
        Assert.Equal("127.0.0.1", aliceReady.Host);
        Assert.Equal(32, aliceReady.Key.Length);
        var aliceState = (await a1.ExpectAsync(Kind.VoiceState)).VoiceState;
        Assert.Equal(voiceId, aliceState.ChannelId);
        var self = Assert.Single(aliceState.Members);
        Assert.Equal(alice.UserId, self.UserId);
        Assert.Equal(aliceReady.Ssrc, self.Ssrc);

        // @everyone grants SPEAK and not PRIORITY_SPEAKER, so an ordinary joiner is neither muted
        // nor a priority speaker.
        Assert.False(self.ServerMuted);
        Assert.False(self.ServerDeafened);
        Assert.False(self.Priority);

        await b1.SendAsync(Frames.JoinVoice(voiceId));
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
    public async Task Voice_joins_and_leaves_are_announced_to_every_viewer_of_the_channel()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob");
        var (alice, a1, b1) = (party.Account(0), party.Client(0), party.Client(1));
        var voiceId = party.GeneralVoiceId;

        await a1.SendAsync(Frames.JoinVoice(voiceId));
        var ready = (await a1.ExpectAsync(Kind.VoiceReady)).VoiceReady;
        await a1.ExpectAsync(Kind.VoiceState);

        // Bob is not in voice and still hears it: the audience is every member who may view the
        // channel. The joiner itself is not told, its own VoiceState having said as much.
        var joined = (await b1.ExpectAsync(Kind.VoiceMemberJoined)).VoiceMemberJoined;
        Assert.Equal(voiceId, joined.ChannelId);
        Assert.Equal(alice.UserId, joined.Member.UserId);
        Assert.Equal(alice.Username, joined.Member.Username);
        Assert.Equal(ready.Ssrc, joined.Member.Ssrc);
        await a1.QuietAsync(WsClient.VoiceKinds);

        // A leave reaches the leaver too.
        await a1.SendAsync(Frames.LeaveVoice(voiceId));
        foreach (var client in new[] { a1, b1 })
        {
            var left = (await client.ExpectAsync(Kind.VoiceMemberLeft)).VoiceMemberLeft;
            Assert.Equal(voiceId, left.ChannelId);
            Assert.Equal(alice.UserId, left.UserId);
        }
    }

    [Fact]
    public async Task JoinVoice_on_a_text_channel_answers_INVALID_ARGUMENT()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var a1 = await WsClient.ConnectAsync(Server, alice);

        // general is a text channel: it holds messages and no voice session at all.
        Assert.Equal(ChannelKind.Text, a1.Session.General.Kind);
        await a1.SendAsync(Frames.JoinVoice(a1.Session.GeneralId));
        await a1.ExpectErrorAsync(ErrorCode.InvalidArgument, fatal: false);
        await a1.QuietAsync(WsClient.VoiceKinds);
    }

    [Fact]
    public async Task LeaveVoice_while_not_in_the_channel_answers_NOT_IN_VOICE()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var a1 = await WsClient.ConnectAsync(Server, alice);
        await a1.SendAsync(Frames.LeaveVoice(a1.Session.GeneralVoiceId));
        await a1.ExpectErrorAsync(ErrorCode.NotInVoice, fatal: false);
    }

    // PROTOCOL.md § Voice: a joiner without SPEAK is muted rather than refused, and the flag is on
    // its session from the first packet.
    [Fact]
    public async Task A_member_without_SPEAK_joins_server_muted()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "bob");
        var (bob, b1) = (party.Account(1), party.Client(1));
        var (_, channelId) = await party.CreateChannelAsync(ChannelKind.Voice);
        await party.SetRoleOverrideAsync(channelId, party.EveryoneId, deny: (ulong)Perm.Speak);

        await b1.SendAsync(Frames.JoinVoice(channelId));
        var ready = (await b1.ExpectAsync(Kind.VoiceReady)).VoiceReady;
        var member = Assert.Single((await b1.ExpectAsync(Kind.VoiceState)).VoiceState.Members);
        Assert.Equal(bob.UserId, member.UserId);
        Assert.True(member.ServerMuted);
        Assert.False(member.Priority);

        // The channel's other viewers are told the same flag.
        var joined = (await party.Client(0).ExpectAsync(Kind.VoiceMemberJoined)).VoiceMemberJoined;
        Assert.Equal(channelId, joined.ChannelId);
        Assert.Equal(ready.Ssrc, joined.Member.Ssrc);
        Assert.True(joined.Member.ServerMuted);
    }

    // PRIORITY_SPEAKER is resolved in the channel, so it is a role the member holds rather than a
    // flag a client sets.
    [Fact]
    public async Task PRIORITY_SPEAKER_in_the_channel_marks_the_voice_member()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "bob");
        var (bob, b1) = (party.Account(1), party.Client(1));
        var (_, channelId) = await party.CreateChannelAsync(ChannelKind.Voice);

        // Granted before the join: the flags are read when the session is created.
        await party.GrantRoleAsync(1, (ulong)Perm.PrioritySpeaker);

        await b1.SendAsync(Frames.JoinVoice(channelId));
        var ready = (await b1.ExpectAsync(Kind.VoiceReady)).VoiceReady;
        var member = Assert.Single((await b1.ExpectAsync(Kind.VoiceState)).VoiceState.Members);
        Assert.Equal(bob.UserId, member.UserId);
        Assert.True(member.Priority);
        Assert.False(member.ServerMuted);

        var joined = (await party.Client(0).ExpectAsync(Kind.VoiceMemberJoined)).VoiceMemberJoined;
        Assert.Equal(ready.Ssrc, joined.Member.Ssrc);
        Assert.True(joined.Member.Priority);
    }

    [Fact]
    public async Task JoinVoice_answers_VOICE_UNAVAILABLE_when_the_relay_is_disabled()
    {
        var server = await fixture.VoiceDisabledAsync();
        var alice = await Accounts.RegisterAsync(server, "alice");
        await using var a1 = await WsClient.ConnectAsync(server, alice);
        await a1.SendAsync(Frames.JoinVoice(a1.Session.GeneralVoiceId));
        await a1.ExpectErrorAsync(ErrorCode.VoiceUnavailable, fatal: false);
    }
}
