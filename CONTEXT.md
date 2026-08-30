# Scrozz

Scrozz captures visual source material and lets people prepare it for sharing
without losing the original composition.

## Language

**Source**:
The untouched pixels and composition produced by capture.
_Avoid_: Original after it has been edited, background

**Scene**:
The singular editable presentation treatment surrounding a Source.
_Avoid_: Smart Frame, Beautification, frame

**Scene canvas**:
The area a Scene adds outside the Source for spacing, placement, and background.
_Avoid_: Crop, matte

**Automatic property**:
A Scene property whose value is chosen from the current Source rather than fixed
by the author.
_Avoid_: Default, random

**Resolved value**:
The visible concrete value currently produced by an Automatic property.
_Avoid_: Hidden default

**Native appearance**:
The captured silhouette, corners, and shadow of a window, treated as part of the
Source rather than editable Scene styling.
_Avoid_: Locked style, synthetic shadow

**Remove Scene**:
The action that returns the document to its Source with no Scene.
_Avoid_: Clear

**Clear canvas**:
The action that keeps the Scene canvas and makes its background transparent.
_Avoid_: Remove Scene

**Scene preset**:
A reusable named mix of Automatic and fixed Scene properties.
_Avoid_: Template, default
