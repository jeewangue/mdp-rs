# Diagram Rendering Fixture

Exercises every diagram path mdp handles so a headless browser can confirm each
one renders to a visible image. Four fences produce five images (the last fence
holds two diagrams).

## Sequence diagram

A named `@startuml` block — the name token must be harmless under `-pipe`.

```plantuml
@startuml sequence-demo
actor User
participant Server
User -> Server : request
Server --> User : response
@enduml
```

## Component diagram

```plantuml
@startuml component-demo
[Frontend] --> [API]
[API] --> [Database]
@enduml
```

## Network diagram (nwdiag)

A non-UML sub-language: must pass through verbatim, not get wrapped in
`@startuml`.

```plantuml
@startnwdiag
nwdiag {
  network dmz {
    address = "10.0.0.0/24"
    web01 [address = "10.0.0.1"];
    web02 [address = "10.0.0.2"];
  }
  network internal {
    address = "10.1.0.0/24"
    web01;
    db01 [address = "10.1.0.1"];
  }
}
@endnwdiag
```

## Two diagrams in one fence

Both must render as separate images instead of one concatenated, invalid SVG.

```plantuml
@startuml multi-a
Alice -> Bob : ping
@enduml
@startuml multi-b
Carol -> Dave : pong
@enduml
```
