/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://svgwg.org/svg2-draft/struct.html#InterfaceSVGSVGElement
[Exposed=Window]
interface SVGSVGElement : SVGGraphicsElement {

  //[SameObject] readonly attribute SVGAnimatedLength x;
  //[SameObject] readonly attribute SVGAnimatedLength y;
  //[SameObject] readonly attribute SVGAnimatedLength width;
  //[SameObject] readonly attribute SVGAnimatedLength height;

  //attribute float currentScale;
  //[SameObject] readonly attribute DOMPointReadOnly currentTranslate;

  //NodeList getIntersectionList(DOMRectReadOnly rect, SVGElement? referenceElement);
  //NodeList getEnclosureList(DOMRectReadOnly rect, SVGElement? referenceElement);
  //boolean checkIntersection(SVGElement element, DOMRectReadOnly rect);
  //boolean checkEnclosure(SVGElement element, DOMRectReadOnly rect);

  //void deselectAll();

  //SVGNumber createSVGNumber();
  //SVGLength createSVGLength();
  //SVGAngle createSVGAngle();
  DOMPoint createSVGPoint();
  DOMMatrix createSVGMatrix();
  DOMRect createSVGRect();
  //SVGTransform createSVGTransform();
  //SVGTransform createSVGTransformFromMatrix(DOMMatrixReadOnly matrix);

  Element? getElementById(DOMString elementId);

  // Deprecated methods that have no effect when called,
  // but which are kept for compatibility reasons.
  unsigned long suspendRedraw(unsigned long maxWaitMilliseconds);
  undefined unsuspendRedraw(unsigned long suspendHandleID);
  undefined unsuspendRedrawAll();
  undefined forceRedraw();
};

// The document time container methods from SVG 1.1, still exposed by every engine.
// https://www.w3.org/TR/SVG11/struct.html#InterfaceSVGSVGElement
partial interface SVGSVGElement {
  undefined pauseAnimations();
  undefined unpauseAnimations();
  boolean animationsPaused();
  float getCurrentTime();
  undefined setCurrentTime(float seconds);
};

//SVGSVGElement includes SVGFitToViewBox;
//SVGSVGElement includes SVGZoomAndPan;
//SVGSVGElement includes WindowEventHandlers;
