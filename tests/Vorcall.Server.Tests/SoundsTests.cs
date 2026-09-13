using System.Buffers.Binary;
using System.Net;
using System.Net.Http.Headers;
using Microsoft.EntityFrameworkCore;
using Microsoft.Extensions.DependencyInjection;
using Vorcall.Server.Api;
using Vorcall.Server.Attachments;
using Vorcall.Server.Permissions;
using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

// PROTOCOL.md § Sounds: one shared library every member sees whole, uploaded and read back over
// REST, triggered over the socket into a voice session that the server never carries a byte of.
[Collection(ServerCollection.Name)]
public sealed class SoundsTests(ServerFixture fixture)
{
    // Small enough to keep the fixtures readable; the server never decodes a packet, so the bytes
    // inside one need not be Opus.
    private const int PacketBytes = 4;

    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Only_MANAGE_SOUNDS_may_add_a_clip_and_its_duration_is_its_frame_count()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "sonia");
        var (owner, sonia) = (party.Account(0), party.Account(1));

        // @everyone grants SOUNDPAD and not MANAGE_SOUNDS, so an ordinary member may trigger a
        // clip and never add one.
        var refused = await Uploads.UploadSoundAsync(Server, sonia.Access, "airhorn", Container(3));
        Assert.Equal(HttpStatusCode.Forbidden, refused.Status);
        Assert.Equal(PermNames.Name(Perm.ManageSounds), refused.Detail);

        // The owner bypasses every check. 3 frames of 20 ms is the duration, taken from the
        // container's header without decoding a sample.
        var uploaded = await Uploads.UploadSoundAsync(Server, owner.Access, "airhorn", Container(3));
        Assert.Equal(HttpStatusCode.Created, uploaded.Status);
        var sound = uploaded.As(Sound.Parser);
        Assert.True(sound.Id > 0);
        Assert.Equal("airhorn", sound.Name);
        Assert.Equal(owner.UserId, sound.UploaderId);
        Assert.Equal(60u, sound.DurationMs);
        Assert.Equal(Container(3).LongLength, sound.Size);

        await ConsumeUpsertedAsync(party, sound.Id);

        // A member holding the bit outright, rather than the owner's bypass, may add one too.
        await party.GrantRoleAsync(1, (ulong)Perm.ManageSounds);
        var granted = await Uploads.UploadSoundAsync(Server, sonia.Access, "yay", Container(10));
        Assert.Equal(HttpStatusCode.Created, granted.Status);
        Assert.Equal(200u, granted.As(Sound.Parser).DurationMs);
        await ConsumeUpsertedAsync(party, granted.As(Sound.Parser).Id);
    }

    [Fact]
    public async Task Every_way_the_VORCSND1_container_can_be_wrong_is_refused()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture);
        var owner = party.Account(0);

        var badMagic = Container(1);
        badMagic[0] = (byte)'X';

        var badRate = Container(1);
        BinaryPrimitives.WriteUInt32LittleEndian(badRate.AsSpan(8, 4), 44100);

        var mono = Container(1);
        mono[12] = 1;

        var reserved = Container(1);
        reserved[13] = 1;

        var noFrames = Container(1);
        BinaryPrimitives.WriteUInt32LittleEndian(noFrames.AsSpan(14, 4), 0);

        // One frame whose declared length runs past the last byte of the body: the walk never
        // lands on the end, so the container is not this one however far it got.
        var overrunning = Container(1);
        BinaryPrimitives.WriteUInt16LittleEndian(overrunning.AsSpan(18, 2), PacketBytes + 8);

        // A byte after the last frame is not part of the container either.
        var trailing = Container(1).Concat<byte>([0x00]).ToArray();

        // Past the ten-minute ceiling of 30000 frames, which the header alone settles.
        var tooManyFrames = Container(1);
        BinaryPrimitives.WriteUInt32LittleEndian(tooManyFrames.AsSpan(14, 4), AttachmentsOptions.SoundMaxFrames + 1u);

        // A frame carrying no packet at all.
        var emptyFrame = Container(1);
        BinaryPrimitives.WriteUInt16LittleEndian(emptyFrame.AsSpan(18, 2), 0);

        // One byte past the largest Opus packet the container may hold.
        var hugeFrame = Container(1);
        BinaryPrimitives.WriteUInt16LittleEndian(hugeFrame.AsSpan(18, 2), AttachmentsOptions.SoundMaxPacketBytes + 1);

        // Three frames' worth of bytes under a header declaring four: the walk runs out of body
        // mid-way, so it never lands on the end.
        var truncatedTail = Container(3);
        BinaryPrimitives.WriteUInt32LittleEndian(truncatedTail.AsSpan(14, 4), 4);

        var bodies = new (string Case, byte[] Body)[]
        {
            ("bad magic", badMagic),
            ("wrong sample rate", badRate),
            ("wrong channel count", mono),
            ("non-zero reserved", reserved),
            ("zero frames", noFrames),
            ("too many frames", tooManyFrames),
            ("zero-length frame", emptyFrame),
            ("frame length past the packet ceiling", hugeFrame),
            ("frame length past the end", overrunning),
            ("truncated tail", truncatedTail),
            ("trailing bytes", trailing),
        };

        // One name for the whole run, so the table below can be looked up in the library by it.
        var name = $"broken {Names.Token()}";
        foreach (var (label, body) in bodies)
        {
            var refused = await Uploads.UploadSoundAsync(Server, owner.Access, name, body);
            Assert.True(refused.Status == HttpStatusCode.BadRequest, $"{label}: {(int)refused.Status}");
            Assert.Equal(SoundsEndpoints.MalformedDetail, refused.Detail);
        }

        // None of them left a row behind for the library to carry — neither a broadcast nor the
        // row the upload writes before it reads the first byte.
        await party.Client(0).QuietAsync();
        var contexts = Server.Services.GetRequiredService<IDbContextFactory<Data.AppDbContext>>();
        await using (var db = await contexts.CreateDbContextAsync())
        {
            Assert.Empty(await db.Sounds.AsNoTracking().Where(s => s.Name == name).ToListAsync());
        }
    }

    [Fact]
    public async Task The_upload_refuses_a_name_the_grammar_does_not_accept()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture);
        var owner = party.Account(0);
        var body = Container(2);

        // No ?name= at all: a perfectly good container with nothing to call it, which is about the
        // name and not about the container.
        var nameless = await Proto.PostBytesAsync(
            Server,
            "/api/sounds",
            body,
            AttachmentsOptions.SoundMediaType,
            owner.Access);
        Assert.Equal(HttpStatusCode.BadRequest, nameless.Status);
        Assert.Equal(SoundsEndpoints.NameField, nameless.Detail);

        // 33 scalars is one past the grammar's ceiling, and a name of nothing but whitespace
        // normalises to the empty one.
        foreach (var refusedName in new[] { new string('a', 33), "   ", string.Empty })
        {
            var refused = await Uploads.UploadSoundAsync(Server, owner.Access, refusedName, body);
            Assert.True(refused.Status == HttpStatusCode.BadRequest, $"'{refusedName.Length}': {(int)refused.Status}");
            Assert.Equal(SoundsEndpoints.NameField, refused.Detail);
        }

        await party.Client(0).QuietAsync();
    }

    [Fact]
    public async Task The_upload_refuses_an_oversized_a_lengthless_and_a_mistyped_body()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture);
        var owner = party.Account(0);
        var body = Container(2);

        // Refused from the declared length alone, so the test needs no seventeen megabytes of
        // its own.
        var tooLarge = await Uploads.UploadSoundAsync(
            Server,
            owner.Access,
            "huge",
            body,
            declaredLength: AttachmentsOptions.SoundMaxFileBytes + 1L);
        Assert.Equal(HttpStatusCode.RequestEntityTooLarge, tooLarge.Status);
        Assert.Equal(SoundsEndpoints.TooLargeDetail, tooLarge.Detail);

        // The declared length is what the quota is charged against, so a body without one is
        // refused before it is read.
        var lengthless = await Proto.SendAsync(
            Server,
            HttpMethod.Post,
            "/api/sounds?name=nolength",
            UnmeasurableContent(body),
            owner.Access,
            ServerFixture.ServerKey,
            request => request.Headers.TransferEncodingChunked = true);
        Assert.Equal(HttpStatusCode.LengthRequired, lengthless.Status);
        Assert.Equal(AttachmentsEndpoints.LengthRequiredDetail, lengthless.Detail);

        foreach (var type in new[] { "audio/opus", "application/octet-stream", "image/png" })
        {
            var mistyped = await Uploads.UploadSoundAsync(Server, owner.Access, "wrongtype", body, type);
            Assert.True(mistyped.Status == HttpStatusCode.UnsupportedMediaType, $"{type}: {(int)mistyped.Status}");
            Assert.Equal(SoundsEndpoints.UnsupportedTypeDetail, mistyped.Detail);
        }

        await party.Client(0).QuietAsync();
    }

    [Fact]
    public async Task A_clip_is_read_back_by_any_bearer_with_the_id_as_its_ETag()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "tessa");
        var (owner, tessa) = (party.Account(0), party.Account(1));
        var body = Container(5);

        var uploaded = await Uploads.UploadSoundAsync(Server, owner.Access, "boop", body);
        Assert.Equal(HttpStatusCode.Created, uploaded.Status);
        var sound = uploaded.As(Sound.Parser);
        await ConsumeUpsertedAsync(party, sound.Id);

        // The whole library is already offered to every member, so holding MANAGE_SOUNDS is no
        // part of reading a clip back.
        var served = await Uploads.DownloadSoundAsync(Server, tessa.Access, sound.Id);
        Assert.Equal(HttpStatusCode.OK, served.Status);
        Assert.Equal(body, served.Body);
        Assert.Equal(AttachmentsOptions.SoundMediaType, served.Header("Content-Type"));

        // A clip's bytes never change — a re-upload is a new row — so the tag is the id alone.
        Assert.Equal($"\"{sound.Id}\"", served.Header("ETag"));
        Assert.Equal(["immutable", "max-age=31536000", "private"], served.Parts("Cache-Control"));

        var missing = await Uploads.DownloadSoundAsync(Server, tessa.Access, sound.Id + 1_000_000);
        Assert.Equal(HttpStatusCode.NotFound, missing.Status);
    }

    [Fact]
    public async Task An_upload_upserts_the_clip_to_every_online_member()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "ulla");
        var owner = party.Account(0);

        var uploaded = await Uploads.UploadSoundAsync(Server, owner.Access, "ta-da", Container(4));
        Assert.Equal(HttpStatusCode.Created, uploaded.Status);
        var sound = uploaded.As(Sound.Parser);

        // The library is server-wide, so the delta goes to everyone, the uploader included.
        foreach (var client in party.Clients)
        {
            var upserted = (await client.ExpectAsync(Kind.SoundUpserted)).SoundUpserted.Sound;
            Assert.Equal(sound.Id, upserted.Id);
            Assert.Equal("ta-da", upserted.Name);
            Assert.Equal(owner.UserId, upserted.UploaderId);
            Assert.Equal(80u, upserted.DurationMs);
            Assert.Equal(sound.Size, upserted.Size);
        }
    }

    [Fact]
    public async Task PlaySound_needs_the_bit_a_live_voice_session_and_a_clip_that_exists()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "vida");
        var (owner, v1) = (party.Account(0), party.Client(1));
        var sound = await UploadAsync(party, owner, frames: 3);

        // A channel where @everyone is denied SOUNDPAD: the check runs before the voice one, so
        // the caller does not even have to be in the session to be refused.
        var (_, mutedChannel) = await party.CreateChannelAsync(ChannelKind.Voice);
        await party.SetRoleOverrideAsync(mutedChannel, party.EveryoneId, deny: (ulong)Perm.Soundpad);
        await v1.SendAsync(Frames.PlaySound(mutedChannel, sound.Id));
        var denied = await v1.ExpectErrorAsync(ErrorCode.PermissionDenied, fatal: false);
        Assert.Equal(PermNames.Name(Perm.Soundpad), denied.Detail);

        // SOUNDPAD is an @everyone default, so on an ordinary voice channel what is missing is
        // the session itself.
        await v1.SendAsync(Frames.PlaySound(party.GeneralVoiceId, sound.Id));
        await v1.ExpectErrorAsync(ErrorCode.NotInVoice, fatal: false);

        // In voice, and an id no clip carries.
        await v1.SendAsync(Frames.JoinVoice(party.GeneralVoiceId));
        await v1.SendAsync(Frames.PlaySound(party.GeneralVoiceId, sound.Id + 1_000_000));
        await v1.ExpectErrorAsync(ErrorCode.UnknownSound, fatal: false);

        // A channel nothing names answers like one the caller may not see.
        await v1.SendAsync(Frames.PlaySound(party.GeneralVoiceId + 1_000_000, sound.Id));
        await v1.ExpectErrorAsync(ErrorCode.UnknownChannel, fatal: false);
    }

    [Fact]
    public async Task SoundPlayed_reaches_every_viewer_of_the_channel_and_the_caller()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "wilma");
        var (owner, wilma) = (party.Account(0), party.Account(1));
        var (o1, w1) = (party.Client(0), party.Client(1));
        var sound = await UploadAsync(party, owner, frames: 3);

        await w1.SendAsync(Frames.JoinVoice(party.GeneralVoiceId));
        await w1.SendAsync(Frames.PlaySound(party.GeneralVoiceId, sound.Id));

        // The owner is not in the session and still hears it: the audience is every member who
        // may view the channel, and the caller plays the clip itself.
        foreach (var client in new[] { w1, o1 })
        {
            var played = (await client.ExpectAsync(Kind.SoundPlayed)).SoundPlayed;
            Assert.Equal(party.GeneralVoiceId, played.ChannelId);
            Assert.Equal(wilma.UserId, played.UserId);
            Assert.Equal(sound.Id, played.SoundId);
        }
    }

    [Fact]
    public async Task A_second_PlaySound_cuts_the_first_without_a_SoundStopped()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "xali");
        var (owner, o1, x1) = (party.Account(0), party.Client(0), party.Client(1));
        var first = await UploadAsync(party, owner, frames: 3);
        var second = await UploadAsync(party, owner, frames: 5);

        await x1.SendAsync(Frames.JoinVoice(party.GeneralVoiceId));
        await x1.SendAsync(Frames.PlaySound(party.GeneralVoiceId, first.Id));
        await x1.SendAsync(Frames.PlaySound(party.GeneralVoiceId, second.Id));

        // A SoundPlayed means "stop whatever this channel was playing, start this", so the cut is
        // the second frame and nothing else is sent for it.
        foreach (var client in new[] { x1, o1 })
        {
            Assert.Equal(first.Id, (await client.ExpectAsync(Kind.SoundPlayed)).SoundPlayed.SoundId);
            Assert.Equal(second.Id, (await client.ExpectAsync(Kind.SoundPlayed)).SoundPlayed.SoundId);
            await client.QuietAsync();
        }
    }

    [Fact]
    public async Task Stopping_belongs_to_the_member_who_started_the_clip_and_to_MANAGE_SOUNDS()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "yuna", "zelia");
        var (owner, o1) = (party.Account(0), party.Client(0));
        var (yuna, y1, z1) = (party.Account(1), party.Client(1), party.Client(2));
        // A minute long: the assertions below, QuietAsync's own second included, must not outlast
        // the clip, or a stop would be the silent success of one that had already finished.
        var sound = await UploadAsync(party, owner, frames: 3000);
        var voiceId = party.GeneralVoiceId;

        await y1.SendAsync(Frames.JoinVoice(voiceId));
        await y1.SendAsync(Frames.PlaySound(voiceId, sound.Id));
        await ExpectPlayedAsync(party, voiceId, yuna.UserId, sound.Id);

        // A third party holding neither the clip nor MANAGE_SOUNDS may not cut it.
        await z1.SendAsync(Frames.StopSound(voiceId));
        var denied = await z1.ExpectErrorAsync(ErrorCode.PermissionDenied, fatal: false);
        Assert.Equal(PermNames.Name(Perm.ManageSounds), denied.Detail);
        foreach (var client in party.Clients)
        {
            await client.QuietAsync();
        }

        // Whoever started it may.
        await y1.SendAsync(Frames.StopSound(voiceId));
        await ExpectStoppedAsync(party, voiceId);

        // And so may a holder of MANAGE_SOUNDS over somebody else's clip.
        await y1.SendAsync(Frames.PlaySound(voiceId, sound.Id));
        await ExpectPlayedAsync(party, voiceId, yuna.UserId, sound.Id);
        await o1.SendAsync(Frames.StopSound(voiceId));
        await ExpectStoppedAsync(party, voiceId);
    }

    [Fact]
    public async Task StopSound_on_a_channel_playing_nothing_is_a_silent_success()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "aria");
        var a1 = party.Client(1);

        await a1.SendAsync(Frames.JoinVoice(party.GeneralVoiceId));
        await a1.SendAsync(Frames.StopSound(party.GeneralVoiceId));

        // Idempotent: no error to the caller and no frame to the channel.
        await a1.PingFenceAsync(Frames.NowMs());
        foreach (var client in party.Clients)
        {
            await client.QuietAsync();
        }
    }

    [Fact]
    public async Task Renaming_and_deleting_need_MANAGE_SOUNDS_and_reach_everyone()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "bela");
        var (owner, o1, b1) = (party.Account(0), party.Client(0), party.Client(1));
        var sound = await UploadAsync(party, owner, frames: 2);

        foreach (var frame in new[] { Frames.UpdateSound(sound.Id, "stolen"), Frames.DeleteSound(sound.Id) })
        {
            await b1.SendAsync(frame);
            var denied = await b1.ExpectErrorAsync(ErrorCode.PermissionDenied, fatal: false);
            Assert.Equal(PermNames.Name(Perm.ManageSounds), denied.Detail);
        }

        // An id no clip carries is refused after the bit is checked, never before it.
        await o1.SendAsync(Frames.UpdateSound(sound.Id + 1_000_000, "ghost"));
        await o1.ExpectErrorAsync(ErrorCode.UnknownSound, fatal: false);
        await o1.SendAsync(Frames.DeleteSound(sound.Id + 1_000_000));
        await o1.ExpectErrorAsync(ErrorCode.UnknownSound, fatal: false);

        // A name the grammar refuses is answered as such, not as an unknown clip: the same
        // "name" the REST upload answers with.
        await o1.SendAsync(Frames.UpdateSound(sound.Id, string.Empty));
        var badName = await o1.ExpectErrorAsync(ErrorCode.InvalidArgument, fatal: false);
        Assert.Equal(SoundsEndpoints.NameField, badName.Detail);

        await o1.SendAsync(Frames.UpdateSound(sound.Id, "  renamed  "));
        foreach (var client in party.Clients)
        {
            var upserted = (await client.ExpectAsync(Kind.SoundUpserted)).SoundUpserted.Sound;
            Assert.Equal(sound.Id, upserted.Id);
            Assert.Equal("renamed", upserted.Name);
            Assert.Equal(sound.DurationMs, upserted.DurationMs);
        }

        await o1.SendAsync(Frames.DeleteSound(sound.Id));
        foreach (var client in party.Clients)
        {
            Assert.Equal(sound.Id, (await client.ExpectAsync(Kind.SoundDeleted)).SoundDeleted.SoundId);
        }

        // Row and file together: the bytes are gone with the library entry.
        var gone = await Uploads.DownloadSoundAsync(Server, owner.Access, sound.Id);
        Assert.Equal(HttpStatusCode.NotFound, gone.Status);
    }

    [Fact]
    public async Task The_hello_snapshot_carries_the_whole_library()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture);
        var owner = party.Account(0);
        var sound = await UploadAsync(party, owner, frames: 7);

        var reader = await Accounts.RegisterAsync(Server, "carmo");
        await using var client = await WsClient.ConnectAsync(Server, reader);

        // Server-wide and unfiltered: a member who holds nothing but @everyone still sees every
        // clip, because triggering one is what SOUNDPAD gates, not knowing of it.
        var listed = Assert.Single(client.Session.Snapshot.Sounds, entry => entry.Id == sound.Id);
        Assert.Equal(sound.Name, listed.Name);
        Assert.Equal(owner.UserId, listed.UploaderId);
        Assert.Equal(140u, listed.DurationMs);
        Assert.Equal(sound.Size, listed.Size);
    }

    [Fact]
    public async Task An_upload_that_never_finished_is_in_no_snapshot_and_serves_no_bytes()
    {
        var contexts = Server.Services.GetRequiredService<IDbContextFactory<Data.AppDbContext>>();
        var store = Server.Services.GetRequiredService<SoundStore>();
        var body = Container(3);

        // The row a process killed mid-upload leaves behind: written before its bytes, and never
        // marked complete. Its file is put in place too, so the only thing refusing it is the flag.
        long id;
        await using (var db = await contexts.CreateDbContextAsync())
        {
            var row = new Data.Sound
            {
                Name = "half",
                UploaderId = null,
                ContentType = AttachmentsOptions.SoundMediaType,
                Size = body.LongLength,
                DurationMs = 60,
                Complete = false,
                CreatedAt = DateTime.UtcNow,
            };
            db.Sounds.Add(row);
            await db.SaveChangesAsync();
            id = row.Id;
        }

        var path = store.PathFor(id);
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        await File.WriteAllBytesAsync(path, body);

        var reader = await Accounts.RegisterAsync(Server, "dinah");
        await using var client = await WsClient.ConnectAsync(Server, reader);
        Assert.DoesNotContain(client.Session.Snapshot.Sounds, entry => entry.Id == id);

        var served = await Uploads.DownloadSoundAsync(Server, reader.Access, id);
        Assert.Equal(HttpStatusCode.NotFound, served.Status);

        // The sweeper takes the row and its file once the short cutoff has passed; an upload still
        // in flight is younger than that and stays.
        Assert.True(await store.SweepIncompleteAsync(DateTime.UtcNow.AddHours(1), CancellationToken.None) >= 1);
        await using (var db = await contexts.CreateDbContextAsync())
        {
            Assert.Null(await db.Sounds.AsNoTracking().FirstOrDefaultAsync(s => s.Id == id));
        }

        Assert.False(File.Exists(path));
    }

    // A VORCSND1 container: the 18-byte header of PROTOCOL.md § Sounds, then one length-prefixed
    // packet per frame. The server never decodes a packet, so these bytes need not be Opus.
    private static byte[] Container(int frames)
    {
        var body = new byte[AttachmentsOptions.SoundHeaderBytes + (frames * (2 + PacketBytes))];
        "VORCSND1"u8.CopyTo(body);
        BinaryPrimitives.WriteUInt32LittleEndian(body.AsSpan(8, 4), 48000);
        body[12] = 2;
        body[13] = 0;
        BinaryPrimitives.WriteUInt32LittleEndian(body.AsSpan(14, 4), (uint)frames);

        var at = AttachmentsOptions.SoundHeaderBytes;
        for (var frame = 0; frame < frames; frame++)
        {
            BinaryPrimitives.WriteUInt16LittleEndian(body.AsSpan(at, 2), PacketBytes);
            at += 2;
            for (var i = 0; i < PacketBytes; i++)
            {
                body[at + i] = (byte)(frame + i);
            }

            at += PacketBytes;
        }

        return body;
    }

    // A body whose length the client cannot work out; sent chunked, since a test host handed a
    // measurable body computes the header this endpoint has to do without.
    private static HttpContent UnmeasurableContent(byte[] body)
    {
        var content = new StreamContent(new UnmeasurableStream(body));
        content.Headers.ContentType = MediaTypeHeaderValue.Parse(AttachmentsOptions.SoundMediaType);
        return content;
    }

    // A clip in the library, with the upsert it broadcast already taken off every inbox: the
    // starting point of every test that is about triggering one rather than adding it.
    private async Task<Sound> UploadAsync(Party party, Account uploader, int frames)
    {
        var response = await Uploads.UploadSoundAsync(Server, uploader.Access, $"clip {Names.Token()}", Container(frames));
        Assert.Equal(HttpStatusCode.Created, response.Status);
        var sound = response.As(Sound.Parser);
        await ConsumeUpsertedAsync(party, sound.Id);
        return sound;
    }

    // Every online member is told about a new clip; a test that is not about the broadcast still
    // has to take it off the inboxes.
    private static async Task ConsumeUpsertedAsync(Party party, long soundId)
    {
        foreach (var client in party.Clients)
        {
            Assert.Equal(soundId, (await client.ExpectAsync(Kind.SoundUpserted)).SoundUpserted.Sound.Id);
        }
    }

    private static async Task ExpectPlayedAsync(Party party, long channelId, long userId, long soundId)
    {
        foreach (var client in party.Clients)
        {
            var played = (await client.ExpectAsync(Kind.SoundPlayed)).SoundPlayed;
            Assert.Equal(channelId, played.ChannelId);
            Assert.Equal(userId, played.UserId);
            Assert.Equal(soundId, played.SoundId);
        }
    }

    private static async Task ExpectStoppedAsync(Party party, long channelId)
    {
        foreach (var client in party.Clients)
        {
            Assert.Equal(channelId, (await client.ExpectAsync(Kind.SoundStopped)).SoundStopped.ChannelId);
        }
    }

    private sealed class UnmeasurableStream(byte[] body) : MemoryStream(body)
    {
        public override bool CanSeek => false;
    }
}
