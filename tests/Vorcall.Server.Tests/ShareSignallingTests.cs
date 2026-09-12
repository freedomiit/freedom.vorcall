using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

// PROTOCOL.md § Screen share: the signalling only, with no media on the wire. A share rides a voice
// session, so every frame here names the voice channel the seed puts beside general — never the
// general text channel, which has no voice session to ride.
[Collection(ServerCollection.Name)]
public sealed class ShareSignallingTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task StartShare_outside_voice_answers_NOT_IN_VOICE()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        await party.Client(0).SendAsync(Frames.StartShare(party.GeneralVoiceId, audio: true));
        await party.Client(0).ExpectErrorAsync(ErrorCode.NotInVoice, fatal: false);
        await party.Client(1).QuietAsync(WsClient.ShareKinds);
        await party.Client(2).QuietAsync(WsClient.ShareKinds);
    }

    [Fact]
    public async Task StartShare_reaches_the_whole_channel_and_a_rejoin_learns_the_running_share()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, bob, carol, c1) = (party.Account(0), party.Client(0), party.Account(1), party.Account(2), party.Client(2));
        var voiceId = party.GeneralVoiceId;
        await JoinVoiceAsync(party);

        await a1.SendAsync(Frames.StartShare(voiceId, audio: true));
        await StartedEverywhereAsync(party, sharer: 0, audio: true, count: 0);

        await c1.SendAsync(Frames.LeaveVoice(voiceId));
        await c1.ExpectAsync(Kind.VoiceMemberLeft);
        await c1.SendAsync(Frames.JoinVoice(voiceId));
        var frames = await c1.ExpectSequenceAsync(Kind.VoiceReady, Kind.VoiceState);
        var members = frames[1].VoiceState.Members.ToDictionary(member => member.UserId);
        Assert.True(members[alice.UserId].Sharing);
        Assert.True(members[alice.UserId].ShareAudio);
        Assert.False(members[bob.UserId].Sharing);
        Assert.False(members[carol.UserId].Sharing);
    }

    [Fact]
    public async Task WatchShare_answers_the_viewer_and_moves_the_sharers_watcher_count()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, bob, b1, carol, c1) = (party.Account(0), party.Client(0), party.Account(1), party.Client(1), party.Account(2), party.Client(2));
        var voiceId = party.GeneralVoiceId;
        await JoinVoiceAsync(party);
        await a1.SendAsync(Frames.StartShare(voiceId, audio: true));
        await StartedEverywhereAsync(party, sharer: 0, audio: true, count: 0);

        await b1.SendAsync(Frames.WatchShare(voiceId, alice.UserId));
        var watch = (await b1.ExpectAsync(Kind.WatchState)).WatchState;
        Assert.Equal((voiceId, alice.UserId), (watch.ChannelId, watch.UserId));
        Assert.Equal(1u, (await a1.ExpectAsync(Kind.ShareWatchers)).ShareWatchers.Count);
        await c1.QuietAsync(WsClient.ShareKinds);

        // A non-sharer, then herself.
        foreach (var target in new[] { bob.UserId, carol.UserId })
        {
            await c1.SendAsync(Frames.WatchShare(voiceId, target));
            await c1.ExpectErrorAsync(ErrorCode.NotSharing, fatal: false);
        }

        await a1.QuietAsync(WsClient.ShareKinds);
        await b1.QuietAsync(WsClient.ShareKinds);

        await c1.SendAsync(Frames.WatchShare(voiceId, alice.UserId));
        Assert.Equal(alice.UserId, (await c1.ExpectAsync(Kind.WatchState)).WatchState.UserId);
        Assert.Equal(2u, (await a1.ExpectAsync(Kind.ShareWatchers)).ShareWatchers.Count);

        await b1.SendAsync(Frames.UnwatchShare(voiceId));
        Assert.Equal(0L, (await b1.ExpectAsync(Kind.WatchState)).WatchState.UserId);
        Assert.Equal(1u, (await a1.ExpectAsync(Kind.ShareWatchers)).ShareWatchers.Count);
    }

    [Fact]
    public async Task StopShare_frees_the_watchers_before_telling_the_channel_and_repeating_it_answers_NOT_SHARING()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, b1, c1) = (party.Account(0), party.Client(0), party.Client(1), party.Client(2));
        var voiceId = party.GeneralVoiceId;
        await JoinVoiceAsync(party);
        await a1.SendAsync(Frames.StartShare(voiceId, audio: true));
        await StartedEverywhereAsync(party, sharer: 0, audio: true, count: 0);
        await c1.SendAsync(Frames.WatchShare(voiceId, alice.UserId));
        await c1.ExpectAsync(Kind.WatchState);
        await a1.ExpectAsync(Kind.ShareWatchers);

        await a1.SendAsync(Frames.StopShare(voiceId));
        var frames = await c1.ExpectSequenceAsync(Kind.WatchState, Kind.ShareStopped);
        Assert.Equal(0L, frames[0].WatchState.UserId);
        Assert.Equal(alice.UserId, frames[1].ShareStopped.UserId);
        foreach (var client in new[] { a1, b1 })
        {
            var stopped = (await client.ExpectAsync(Kind.ShareStopped)).ShareStopped;
            Assert.Equal((voiceId, alice.UserId), (stopped.ChannelId, stopped.UserId));
        }

        await a1.SendAsync(Frames.StopShare(voiceId));
        await a1.ExpectErrorAsync(ErrorCode.NotSharing, fatal: false);
    }

    [Fact]
    public async Task LeaveVoice_while_sharing_sends_ShareStopped_before_VoiceMemberLeft()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (bob, b1) = (party.Account(1), party.Client(1));
        var voiceId = party.GeneralVoiceId;
        await JoinVoiceAsync(party);

        await b1.SendAsync(Frames.StartShare(voiceId, audio: false));
        await StartedEverywhereAsync(party, sharer: 1, audio: false, count: 0);

        await b1.SendAsync(Frames.LeaveVoice(voiceId));
        foreach (var client in party.Clients)
        {
            var frames = await client.ExpectSequenceAsync(Kind.ShareStopped, Kind.VoiceMemberLeft);
            Assert.Equal(bob.UserId, frames[0].ShareStopped.UserId);
            Assert.Equal(bob.UserId, frames[1].VoiceMemberLeft.UserId);
        }
    }

    [Fact]
    public async Task Three_sharers_fit_and_a_repeated_StartShare_re_announces_the_share()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var voiceId = party.GeneralVoiceId;
        await JoinVoiceAsync(party);
        for (var i = 0; i < party.Clients.Count; i++)
        {
            await party.Client(i).SendAsync(Frames.StartShare(voiceId, audio: false));
            await StartedEverywhereAsync(party, sharer: i, audio: false, count: 0);
        }

        // Already sharing, so this neither counts against the channel's ceiling nor resets its
        // watchers.
        await party.Client(0).SendAsync(Frames.StartShare(voiceId, audio: true));
        await StartedEverywhereAsync(party, sharer: 0, audio: true, count: 0);
    }

    [Fact]
    public async Task A_fourth_sharer_answers_SHARE_LIMIT()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol", "dave");
        var voiceId = party.GeneralVoiceId;
        await JoinVoiceAsync(party);
        for (var i = 0; i < 3; i++)
        {
            await party.Client(i).SendAsync(Frames.StartShare(voiceId, audio: false));
            await StartedEverywhereAsync(party, sharer: i, audio: false, count: 0);
        }

        await party.Client(3).SendAsync(Frames.StartShare(voiceId, audio: false));
        await party.Client(3).ExpectErrorAsync(ErrorCode.ShareLimit, fatal: false);
    }

    [Fact]
    public async Task A_replaced_session_stops_its_share_before_its_voice_membership_ends()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, b1, c1) = (party.Account(0), party.Client(1), party.Client(2));
        await JoinVoiceAsync(party);
        await party.Client(0).SendAsync(Frames.StartShare(party.GeneralVoiceId, audio: true));
        await StartedEverywhereAsync(party, sharer: 0, audio: true, count: 0);

        await party.ReconnectAsync(Server, 0);
        foreach (var client in new[] { b1, c1 })
        {
            var frames = await client.ExpectSequenceAsync(Kind.ShareStopped, Kind.VoiceMemberLeft);
            Assert.Equal(alice.UserId, frames[0].ShareStopped.UserId);
            Assert.Equal(alice.UserId, frames[1].VoiceMemberLeft.UserId);
        }

        await party.Client(0).QuietAsync(WsClient.ShareKinds);
    }

    [Fact]
    public async Task StartShare_answers_SHARE_UNAVAILABLE_when_sharing_is_disabled()
    {
        var server = await fixture.ShareDisabledAsync();
        var alice = await Accounts.RegisterAsync(server, "alice");
        await using var a1 = await WsClient.ConnectAsync(server, alice);
        var voiceId = a1.Session.GeneralVoiceId;

        // Voice itself keeps working with the kill switch on.
        await a1.SendAsync(Frames.JoinVoice(voiceId));
        await a1.ExpectAsync(Kind.VoiceReady);
        await a1.ExpectAsync(Kind.VoiceState);

        await a1.SendAsync(Frames.StartShare(voiceId, audio: false));
        await a1.ExpectErrorAsync(ErrorCode.ShareUnavailable, fatal: false);
    }

    // Every member joins the seeded voice channel in party order, each with an ssrc of its own. The
    // joiner hears VoiceReady then VoiceState; the members already in are told by VoiceMemberJoined,
    // which the next joiner's own reads leave behind.
    private static async Task JoinVoiceAsync(Party party)
    {
        var voiceId = party.GeneralVoiceId;
        var ssrcs = new List<uint>();
        VoiceState? state = null;
        for (var i = 0; i < party.Clients.Count; i++)
        {
            var (account, client) = (party.Account(i), party.Client(i));
            await client.SendAsync(Frames.JoinVoice(voiceId));
            var frames = await client.ExpectSequenceAsync(Kind.VoiceReady, Kind.VoiceState);
            var ready = frames[0].VoiceReady;
            Assert.Equal(voiceId, ready.ChannelId);
            Assert.NotEqual(0u, ready.Ssrc);
            ssrcs.Add(ready.Ssrc);
            state = frames[1].VoiceState;
            Assert.Equal(voiceId, state.ChannelId);
            Assert.Contains(state.Members, member => member.UserId == account.UserId);
        }

        Assert.Equal(party.Clients.Count, ssrcs.Distinct().Count());
        Assert.Equal(
            party.Accounts.Select(account => account.UserId).Order().ToArray(),
            state!.Members.Select(member => member.UserId).Order().ToArray());
        Assert.All(state.Members, member => Assert.False(member.Sharing));

        // The joins the earlier members were told about, consumed so that later reads see only the
        // share frames they are about.
        for (var i = 0; i < party.Clients.Count; i++)
        {
            for (var later = i + 1; later < party.Clients.Count; later++)
            {
                var joined = (await party.Client(i).ExpectAsync(Kind.VoiceMemberJoined)).VoiceMemberJoined;
                Assert.Equal(voiceId, joined.ChannelId);
                Assert.Equal(party.Account(later).UserId, joined.Member.UserId);
            }
        }
    }

    // ShareStarted reaches every viewer of the channel, the sharer included; the sharer then hears
    // its watcher count.
    private static async Task StartedEverywhereAsync(Party party, int sharer, bool audio, uint count)
    {
        var voiceId = party.GeneralVoiceId;
        for (var i = 0; i < party.Clients.Count; i++)
        {
            var client = party.Client(i);
            ShareStarted started;
            if (i == sharer)
            {
                var frames = await client.ExpectSequenceAsync(Kind.ShareStarted, Kind.ShareWatchers);
                started = frames[0].ShareStarted;
                Assert.Equal(voiceId, frames[1].ShareWatchers.ChannelId);
                Assert.Equal(count, frames[1].ShareWatchers.Count);
            }
            else
            {
                started = (await client.ExpectAsync(Kind.ShareStarted)).ShareStarted;
            }

            Assert.Equal(voiceId, started.ChannelId);
            Assert.Equal(party.Account(sharer).UserId, started.UserId);
            Assert.Equal(audio, started.Audio);
        }
    }
}
